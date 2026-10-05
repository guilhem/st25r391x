use super::*;

struct TimedScript {
    script: Script,
    commands: Vec<(u8, Instant)>,
}
impl Device for TimedScript {
    fn transfer(
        &mut self,
        p: &[u8],
        r: Option<&mut [u8]>,
    ) -> std::result::Result<u32, LinuxI2CError> {
        let result = self.script.transfer(p, r);
        if p.len() == 1 && p[0] >= 0xc0 {
            self.commands.push((p[0], Instant::now()));
        }
        result
    }
}
#[test]
fn regression_ambiguous_full_type_b_frame_keeps_power_through_eof() {
    for technology in [Technology::NfcB, Technology::St25tb] {
        let payload = [0; 512];
        let mut s = Script::default();
        frame_script(&mut s, &payload, 4096, &[], true, false, None);
        let tx = s.steps.iter().position(|s| s.prefix == [0xc4]).unwrap();
        s.steps.truncate(tx);
        s.fail(&[0xc4], false, 5);
        off(&mut s);
        // Simulate a failed ioctl which returns after the operation deadline.
        s.tx_delay = Duration::from_millis(20);
        let mut r = St25r391x::new(TimedScript {
            script: s,
            commands: Vec::new(),
        });
        r.initialized = true;
        r.field = FieldState::On;
        r.technology = Some(technology);
        let error = r
            .exchange(
                &payload,
                &mut [],
                FrameOptions {
                    tx_only: true,
                    field_hold: Duration::ZERO,
                    ..FrameOptions::default()
                },
                Duration::from_millis(10),
            )
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
        let returned = r
            .device
            .commands
            .iter()
            .find(|(c, _)| *c == 0xc4)
            .unwrap()
            .1;
        let stopped = r.device.commands.last().unwrap();
        assert_eq!(stopped.0, 0xc2);
        // DS11456 §3.1: 512 payload + 2 CRC chars, ten ETU each, then
        // SOF (12 ETU minimum) and EOF (10 ETU). fc/128 is the bit rate.
        let minimum =
            Duration::from_nanos(((514u64 * 10 + 22) * 128 * 1_000_000_000).div_ceil(13_560_000));
        assert!(r.hold_until.duration_since(returned) >= minimum);
        assert!(stopped.1.duration_since(returned) >= minimum);
        assert!(r.device.script.steps.is_empty());
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TbState {
    Ready,
    Inventory,
    Selected,
}
/// Tag state model from DS11456 §6, independent of the selector's bus script.
struct TbDevice {
    state: TbState,
    registers: [u8; 64],
    irq: [u8; 4],
    tx: Vec<u8>,
    rx: Vec<u8>,
    programming_until: Instant,
    premature_command: bool,
}
impl TbDevice {
    fn new() -> Self {
        let mut registers = [0; 64];
        registers[2] = 0xc9;
        Self {
            state: TbState::Ready,
            registers,
            irq: [0; 4],
            tx: Vec::new(),
            rx: Vec::new(),
            programming_until: Instant::now(),
            premature_command: false,
        }
    }
    fn transmit(&mut self) {
        self.irq = [8, 0, 0, 0];
        self.rx.clear();
        if Instant::now() < self.programming_until {
            self.premature_command = true;
        }
        match self.tx.as_slice() {
            [0x0c] => {
                if self.state == TbState::Selected {
                    self.state = TbState::Inventory;
                }
            }
            [6, 0] if self.state != TbState::Selected => {
                self.state = TbState::Inventory;
                self.rx.push(7);
            }
            [0x0e, 7] if self.state == TbState::Inventory => {
                self.state = TbState::Selected;
                self.rx.push(7);
            }
            [0x0b] if self.state == TbState::Selected => {
                self.rx.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8])
            }
            [9, _, _, _, _, _] if self.state == TbState::Selected => {
                self.programming_until = Instant::now() + Duration::from_millis(7);
            }
            _ => {}
        }
        if self.rx.is_empty() {
            self.irq[1] = 0x40;
        } else {
            self.rx.extend_from_slice(&[0, 0]);
            self.irq[0] |= 0x10;
        }
    }
}
impl Device for TbDevice {
    fn transfer(
        &mut self,
        p: &[u8],
        r: Option<&mut [u8]>,
    ) -> std::result::Result<u32, LinuxI2CError> {
        if let Some(out) = r {
            match p {
                [0x5a] => {
                    out.copy_from_slice(&self.irq);
                    self.irq = [0; 4];
                }
                [0x5e] => out.copy_from_slice(&[self.rx.len() as u8, 0]),
                [0x9f] => {
                    out.copy_from_slice(&self.rx);
                    self.rx.clear();
                }
                [0x71] => out[0] = 0,
                [address] if address & 0xc0 == 0x40 => {
                    for (i, b) in out.iter_mut().enumerate() {
                        *b = self.registers[usize::from(address & 0x3f) + i];
                    }
                }
                _ => panic!("unexpected model read: {p:x?}"),
            }
            return Ok(2);
        }
        match p {
            [0xc2] => {
                self.irq = [0; 4];
                self.rx.clear();
            }
            [0xdb] => {
                self.tx.clear();
                self.rx.clear();
            }
            [0xc4] => self.transmit(),
            [0xe8] => {}
            [0x80, data @ ..] => self.tx = data.to_vec(),
            [address, data @ ..] if *address < 0x40 => {
                self.registers[usize::from(*address)..usize::from(*address) + data.len()]
                    .copy_from_slice(data);
                if p == [2, 0] {
                    if Instant::now() < self.programming_until {
                        self.premature_command = true;
                    }
                    self.state = TbState::Ready;
                }
            }
            _ => panic!("unexpected model write: {p:x?}"),
        }
        Ok(1)
    }
}
#[test]
fn regression_tb_discover_then_select_then_discover_restores_inventory() {
    let mut r = St25r391x::new(TbDevice::new());
    r.initialized = true;
    r.field = FieldState::On;
    r.technology = Some(Technology::St25tb);
    let tag = r
        .discover(Technology::St25tb, Duration::from_secs(1))
        .unwrap()
        .unwrap();
    assert_eq!(r.device.state, TbState::Selected);
    assert_eq!(
        r.select(&tag.id(), Duration::from_secs(1)).unwrap().id(),
        tag.id()
    );
    r.exchange(
        &[9, 7, 1, 2, 3, 4],
        &mut [],
        FrameOptions {
            tx_only: true,
            field_hold: Duration::ZERO,
            ..FrameOptions::default()
        },
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(
        r.discover(Technology::St25tb, Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .id(),
        tag.id()
    );
    assert!(!r.device.premature_command);
    assert!(!r.is_poisoned());
    r.shutdown(Duration::from_millis(100)).unwrap();
}

#[test]
fn regression_cl3_88_is_uid_data() {
    let mut s = Script::default();
    frame_script(&mut s, &[], 0, &[0x84, 0], false, false, Some(0xc7));
    // AN10927 fig 1: CL3 holds UID6..UID9, without a cascade tag.
    for (sel, block, sak) in [
        (0x93, [0x88, 4, 1, 2, 0x8f], 4),
        (0x95, [0x88, 3, 4, 5, 0x8a], 4),
        (0x97, [0x88, 7, 8, 9, 0x8e], 0),
    ] {
        frame_script(&mut s, &[sel, 0x20], 16, &block, false, true, None);
        let mut tx = vec![sel, 0x70];
        tx.extend_from_slice(&block);
        frame_script(&mut s, &tx, 56, &[sak, 0, 0], true, false, None);
    }
    off(&mut s);
    let mut r = reader(s);
    let id = TagId::NfcA(vec![4, 1, 2, 3, 4, 5, 0x88, 7, 8, 9]);
    assert_eq!(r.select(&id, Duration::from_secs(1)).unwrap().id(), id);
    r.shutdown(Duration::from_millis(100)).unwrap();
    finish(&mut r);
}
#[test]
fn regression_ats_with_only_length_byte_is_valid() {
    let mut s = Script::default();
    frame_script(&mut s, &[], 0, &[4, 0], false, false, Some(0xc6));
    frame_script(
        &mut s,
        &[0x93, 0x20],
        16,
        &[1, 2, 3, 4, 4],
        false,
        true,
        None,
    );
    frame_script(
        &mut s,
        &[0x93, 0x70, 1, 2, 3, 4, 4],
        56,
        &[0x20, 0, 0],
        true,
        false,
        None,
    );
    frame_script(&mut s, &[0xe0, 0x80], 16, &[1, 0, 0], true, false, None);
    off(&mut s);
    let mut r = reader(s);
    let Some(Tag::NfcA(tag)) = r
        .discover(Technology::NfcA, Duration::from_secs(1))
        .unwrap()
    else {
        panic!("expected NFC-A tag")
    };
    assert_eq!(tag.ats, vec![1]);
    r.shutdown(Duration::from_millis(100)).unwrap();
    finish(&mut r);
}

struct UnavailableRegister {
    calls: usize,
    delay: Duration,
    io_variant: bool,
}
impl Device for UnavailableRegister {
    fn transfer(
        &mut self,
        p: &[u8],
        r: Option<&mut [u8]>,
    ) -> std::result::Result<u32, LinuxI2CError> {
        assert_eq!(p, [0x7f]);
        assert!(r.is_some());
        self.calls += 1;
        thread::sleep(self.delay);
        Err(if self.io_variant {
            LinuxI2CError::Io(std::io::Error::from_raw_os_error(11))
        } else {
            LinuxI2CError::Errno(11)
        })
    }
}
#[test]
fn regression_retry_deadline_preserves_last_linux_error() {
    for io_variant in [false, true] {
        for delay in [Duration::ZERO, Duration::from_millis(20)] {
            let mut r = St25r391x::new(UnavailableRegister {
                calls: 0,
                delay,
                io_variant,
            });
            r.settings.poll_interval = Duration::from_millis(10);
            let error = r
                .reg(0x3f, Instant::now() + Duration::from_millis(5))
                .unwrap_err();
            assert_eq!(r.device.calls, 1);
            assert!(matches!(error.kind, ErrorKind::Deadline));
            assert_eq!(error.raw_os_error(), Some(11));
            assert!(std::error::Error::source(&error).is_some());
            assert_eq!(
                matches!(error.deadline_source, Some(LinuxI2CError::Io(_))),
                io_variant
            );
        }
    }
}
