use super::*;
use std::collections::VecDeque;
struct Step {
    prefix: Vec<u8>,
    reply: Option<Vec<u8>>,
    count: u32,
    errno: Option<i32>,
}
#[derive(Default)]
struct Script {
    steps: VecDeque<Step>,
    tx_delay: Duration,
}
impl Script {
    fn write(&mut self, p: &[u8]) {
        self.steps.push_back(Step {
            prefix: p.to_vec(),
            reply: None,
            count: 1,
            errno: None,
        });
    }
    fn read(&mut self, p: &[u8], r: &[u8]) {
        self.steps.push_back(Step {
            prefix: p.to_vec(),
            reply: Some(r.to_vec()),
            count: 2,
            errno: None,
        });
    }
    fn fail(&mut self, p: &[u8], read: bool, errno: i32) {
        self.steps.push_back(Step {
            prefix: p.to_vec(),
            reply: read.then(Vec::new),
            count: 0,
            errno: Some(errno),
        });
    }
}
impl Device for Script {
    fn transfer(
        &mut self,
        p: &[u8],
        r: Option<&mut [u8]>,
    ) -> std::result::Result<u32, LinuxI2CError> {
        let s = self.steps.pop_front().expect("unexpected transaction");
        if p == [0xc4] {
            thread::sleep(self.tx_delay);
        }
        assert_eq!(p, s.prefix);
        assert_eq!(r.is_some(), s.reply.is_some());
        if let Some(n) = s.errno {
            return Err(LinuxI2CError::Errno(n));
        }
        if let (Some(out), Some(data)) = (r, s.reply) {
            assert_eq!(out.len(), data.len());
            out.copy_from_slice(&data);
        }
        Ok(s.count)
    }
}
fn reader(s: Script) -> St25r391x<Script> {
    let mut r = St25r391x::new(s);
    r.initialized = true;
    r.field = FieldState::On;
    r.technology = Some(Technology::NfcA);
    r
}
fn finish(r: &mut St25r391x<Script>) {
    assert!(r.device.steps.is_empty());
    r.field = FieldState::Off;
}
fn off(s: &mut Script) {
    s.write(&[0xc2]);
    s.write(&[2, 0]);
    s.read(&[0x42], &[0]);
    s.read(&[0x71], &[0]);
}
#[test]
fn exact_bank_selector_and_combined_reads() {
    let mut s = Script::default();
    s.write(&[0xfb, 0x0c, 0x51, 0]);
    s.read(&[0x7f], &[0x29]);
    let mut r = reader(s);
    let d = Instant::now() + Duration::from_secs(1);
    r.bank_b(0x0c, &[0x51, 0], d).unwrap();
    assert_eq!(r.reg(0x3f, d).unwrap(), 0x29);
    finish(&mut r);
}
#[test]
fn only_ordinary_register_reads_retry() {
    let mut s = Script::default();
    s.fail(&[0x7f], true, 11);
    s.read(&[0x7f], &[0x28]);
    let mut r = reader(s);
    assert_eq!(
        r.reg(0x3f, Instant::now() + Duration::from_secs(1))
            .unwrap(),
        0x28
    );
    finish(&mut r);
    for p in [0x5a, 0x9f] {
        let mut s = Script::default();
        s.fail(&[p], true, 11);
        let mut r = reader(s);
        let mut out = [0];
        assert_eq!(
            r.read(
                &[p],
                &mut out,
                false,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .raw_os_error(),
            Some(11)
        );
        finish(&mut r);
    }
}
#[test]
fn fifo_limits_include_crc_and_partial_bits() {
    for (status, len, bits) in [([0, 0x80], 512, 4096), ([1, 6], 1, 3)] {
        let mut s = Script::default();
        s.read(&[0x5e], &status);
        s.read(&[0x9f], &vec![0; len]);
        let mut r = reader(s);
        let mut rx = [0; 512];
        let f = r
            .fifo_read(
                &mut rx,
                &FrameOptions {
                    rx_crc: false,
                    ..FrameOptions::default()
                },
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(f.bits, bits);
        finish(&mut r);
    }
    let mut s = Script::default();
    s.read(&[0x5e], &[0, 0x90]);
    let mut r = reader(s);
    assert!(matches!(
        r.fifo_read(
            &mut [0; 512],
            &FrameOptions::default(),
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap_err()
        .kind,
        ErrorKind::FifoOverflow
    ));
    finish(&mut r);
}
#[test]
fn irq_cache_and_status_errors() {
    let mut s = Script::default();
    s.read(&[0x5a], &[0x18, 0, 0x80, 0]);
    let mut r = reader(s);
    r.irq_update(Instant::now() + Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        r.wait_irq(0, 8, Instant::now() + Duration::from_secs(1))
            .unwrap(),
        8
    );
    assert_eq!(r.irqs[0], 0x10);
    assert!(matches!(
        r.frame_errors(&FrameOptions::default(), false)
            .unwrap_err()
            .kind,
        ErrorKind::Crc
    ));
    r.irqs[2] = 0x40;
    assert!(matches!(
        r.frame_errors(&FrameOptions::default(), false)
            .unwrap_err()
            .kind,
        ErrorKind::Parity
    ));
    finish(&mut r);
}
#[test]
fn cleanup_preserves_errno_and_poisoning() {
    let mut s = Script::default();
    off(&mut s);
    let mut r = reader(s);
    r.stage = Stage::Transmit;
    r.changed = true;
    r.transmission = Transmission::PossiblyStarted;
    let e = r.error(ErrorKind::Transport(LinuxI2CError::Errno(5)));
    let e = r.failure::<()>(e).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(5));
    assert!(e.cleanup.is_none());
    assert_eq!(e.transmission, Transmission::PossiblyStarted);
    assert_eq!(e.field, FieldState::Off);
    assert!(r.is_poisoned());
    assert!(matches!(
        r.start(Duration::from_secs(1)).unwrap_err().kind,
        ErrorKind::Poisoned
    ));
    finish(&mut r);
}
#[test]
fn failed_cleanup_retains_both_errors() {
    let mut s = Script::default();
    s.fail(&[0xc2], false, 6);
    let mut r = reader(s);
    let e = r.error(ErrorKind::Transport(LinuxI2CError::Errno(5)));
    let e = r.failure::<()>(e).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(5));
    assert_eq!(e.cleanup.unwrap().raw_os_error(), Some(6));
    assert_eq!(r.field_state(), FieldState::Unknown);
    finish(&mut r);
}
#[test]
fn deadline_and_validation_do_not_touch_bus() {
    let mut r = reader(Script::default());
    assert!(matches!(
        r.check(Instant::now()).unwrap_err().kind,
        ErrorKind::Deadline
    ));
    assert!(r.validate_frame(&[0], 9, &FrameOptions::default()).is_err());
    assert!(matches!(
        r.select(&TagId::NfcA(vec![1]), Duration::from_secs(1))
            .unwrap_err()
            .kind,
        ErrorKind::InvalidArgument(_)
    ));
    finish(&mut r);
}
#[test]
fn partial_write_is_not_replayed() {
    let mut s = Script::default();
    s.steps.push_back(Step {
        prefix: vec![0xc4],
        reply: None,
        count: 0,
        errno: None,
    });
    let mut r = reader(s);
    let e = r
        .command(0xc4, Instant::now() + Duration::from_secs(1))
        .unwrap_err();
    assert!(matches!(
        e.kind,
        ErrorKind::ShortTransfer {
            expected: 1,
            completed: 0
        }
    ));
    assert!(e.possible_changes);
    finish(&mut r);
}
#[test]
fn shutdown_waits_for_programming_even_without_budget() {
    let mut s = Script::default();
    off(&mut s);
    let mut r = reader(s);
    let start = Instant::now();
    r.hold_until = start + Duration::from_millis(10);
    r.shutdown(Duration::from_millis(10)).unwrap();
    assert!(start.elapsed() >= Duration::from_millis(10));
    finish(&mut r);
}
fn frame_script(
    s: &mut Script,
    tx: &[u8],
    bits: usize,
    data: &[u8],
    crc: bool,
    ant: bool,
    special: Option<u8>,
) {
    s.write(&[0xc2]);
    s.write(&[0xdb]);
    s.write(&[5, u8::from(ant)]);
    s.write(&[0x0a, if crc { 0 } else { 0x80 }]);
    s.write(&[0x12, 0]);
    s.write(&[0x10, 4, 0x24]); // ceil(5ms*13.56MHz/64)=1060
    if special.is_none() && bits != 0 {
        s.write(&[0x22, (bits >> 8) as u8, bits as u8]);
        let mut fifo = vec![0x80];
        fifo.extend_from_slice(&tx[..bits.div_ceil(8)]);
        s.write(&fifo);
    }
    s.read(&[0x5a], &[0; 4]);
    s.write(&[special.unwrap_or(if crc { 0xc4 } else { 0xc5 })]);
    s.read(&[0x5a], &[0x18, 0, 0, 0]);
    s.read(&[0x5e], &[0, 0]);
    s.read(&[0x5a], &[0; 4]);
    s.read(&[0x5e], &[data.len() as u8, ((data.len() >> 8) as u8) << 6]);
    s.read(&[0x9f], data);
    s.write(&[0xe8]);
}
#[test]
fn nfc_a_select_uid_and_invalid_bcc() {
    for bad in [false, true] {
        let mut s = Script::default();
        frame_script(&mut s, &[], 0, &[4, 0], false, false, Some(0xc6));
        frame_script(
            &mut s,
            &[0x93, 0x20],
            16,
            &[1, 2, 3, 4, if bad { 0 } else { 4 }],
            false,
            true,
            None,
        );
        if bad {
            off(&mut s);
        } else {
            frame_script(
                &mut s,
                &[0x93, 0x70, 1, 2, 3, 4, 4],
                56,
                &[0, 0, 0],
                true,
                false,
                None,
            );
        }
        let mut r = reader(s);
        let t = r.discover(Technology::NfcA, Duration::from_secs(1));
        if bad {
            assert!(matches!(
                t.unwrap_err().kind,
                ErrorKind::InvalidResponse("UID BCC")
            ));
        } else {
            assert_eq!(t.unwrap().unwrap().id(), TagId::NfcA(vec![1, 2, 3, 4]));
        }
        finish(&mut r);
    }
}
#[test]
fn nfc_a_cascade_seven_and_ten_byte_uids() {
    for uid in [
        vec![1, 2, 3, 4, 5, 6, 7],
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
    ] {
        let mut s = Script::default();
        frame_script(&mut s, &[], 0, &[0x44, 0], false, false, Some(0xc7));
        for level in 0..(uid.len() - 1) / 3 {
            let sel = [0x93, 0x95, 0x97][level];
            let mut block = [0; 5];
            let pos = level * 3;
            let cascade = uid.len() > pos + 4;
            if cascade {
                block[0] = 0x88;
                block[1..4].copy_from_slice(&uid[pos..pos + 3]);
            } else {
                block[..4].copy_from_slice(&uid[pos..pos + 4]);
            }
            block[4] = block[..4].iter().fold(0, |a, b| a ^ b);
            frame_script(&mut s, &[sel, 0x20], 16, &block, false, true, None);
            let mut tx = vec![sel, 0x70];
            tx.extend_from_slice(&block);
            frame_script(
                &mut s,
                &tx,
                56,
                &[if cascade { 4 } else { 0 }, 0, 0],
                true,
                false,
                None,
            );
        }
        let mut r = reader(s);
        assert_eq!(
            r.select(&TagId::NfcA(uid.clone()), Duration::from_secs(1))
                .unwrap()
                .id(),
            TagId::NfcA(uid)
        );
        finish(&mut r);
    }
}
#[test]
fn nfc_b_pupi_attrib_and_invalid_response() {
    for bad in [false, true] {
        let mut s = Script::default();
        let mut atqb = [0; 14];
        atqb[0] = if bad { 0 } else { 0x50 };
        atqb[1..5].copy_from_slice(&[1, 2, 3, 4]);
        atqb[10] = 1;
        frame_script(&mut s, &[5, 0, 0], 24, &atqb, true, false, None);
        if bad {
            off(&mut s);
        } else {
            frame_script(
                &mut s,
                &[0x1d, 1, 2, 3, 4, 0, 8, 1, 0],
                72,
                &[0x20, 0, 0],
                true,
                false,
                None,
            );
        }
        let mut r = reader(s);
        r.technology = Some(Technology::NfcB);
        let result = r.discover(Technology::NfcB, Duration::from_secs(1));
        if bad {
            assert!(matches!(
                result.unwrap_err().kind,
                ErrorKind::InvalidResponse("ATQB")
            ));
        } else {
            assert_eq!(result.unwrap().unwrap().id(), TagId::NfcB([1, 2, 3, 4]));
        }
        finish(&mut r);
    }
}
#[test]
fn st25tb_initiate_select_uid() {
    let mut s = Script::default();
    tb_inventory_reset_script(&mut s);
    frame_script(&mut s, &[6, 0], 16, &[0xa5, 0, 0], true, false, None);
    frame_script(&mut s, &[0x0e, 0xa5], 16, &[0xa5, 0, 0], true, false, None);
    frame_script(
        &mut s,
        &[0x0b],
        8,
        &[1, 2, 3, 4, 5, 6, 7, 8, 0, 0],
        true,
        false,
        None,
    );
    let mut r = reader(s);
    r.technology = Some(Technology::St25tb);
    assert_eq!(
        r.discover(Technology::St25tb, Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .id(),
        TagId::St25tb([1, 2, 3, 4, 5, 6, 7, 8])
    );
    finish(&mut r);
}
#[test]
fn initialization_identity_and_reset_recovers_poison() {
    let mut s = Script::default();
    s.write(&[0xc0]);
    s.write(&[2, 0]);
    s.read(&[0x42], &[0]);
    s.read(&[0x7f], &[0x29]);
    s.write(&[0, 7]);
    s.write(&[1, 0x84]);
    s.write(&[0x16, 255, 255, 255, 255]);
    s.read(&[0x5a], &[0; 4]);
    s.write(&[2, 0x81]);
    s.read(&[0x5a], &[0x80, 0, 0, 0]);
    s.read(&[0x71], &[0x10]);
    s.write(&[0x2c, 0x80]);
    s.write(&[0x2c, 0]);
    s.read(&[0x5a], &[0; 4]);
    s.write(&[0xd6]);
    s.read(&[0x5a], &[0, 0x80, 0, 0]);
    off(&mut s);
    let mut r = St25r391x::new(s);
    r.poisoned = true;
    assert_eq!(
        r.initialize(Settings::default(), Duration::from_secs(1))
            .unwrap(),
        Identity {
            type_code: 5,
            revision: 1
        }
    );
    assert!(!r.is_poisoned());
    finish(&mut r);
}
#[test]
fn unsupported_modes_do_not_mutate() {
    let mut r = reader(Script::default());
    for t in [Technology::NfcF, Technology::Iso15693] {
        assert!(matches!(
            r.discover(t, Duration::from_secs(1)).unwrap_err().kind,
            ErrorKind::Unsupported(_)
        ));
    }
    finish(&mut r);
}
#[test]
fn nfc_a_collision_advances_nvb_and_selects_target_branch() {
    for target in [false, true] {
        let uid = [if target { 3 } else { 1 }, 2, 3, 4];
        let block = [uid[0], 2, 3, 4, uid.iter().fold(0, |a, b| a ^ b)];
        let mut s = Script::default();
        frame_script(
            &mut s,
            &[],
            0,
            &[4, 0],
            false,
            false,
            Some(if target { 0xc7 } else { 0xc6 }),
        );
        let start = s.steps.len();
        frame_script(&mut s, &[0x93, 0x20], 16, &[1], false, true, None);
        for step in s.steps.iter_mut().skip(start) {
            if step.reply == Some(vec![0x18, 0, 0, 0]) {
                step.reply = Some(vec![0x1c, 0, 0, 0]);
            }
        }
        s.read(&[0x60], &[2]); // one valid bit precedes the collision
        let mut rest = [0; 5];
        for bit in 0..38 {
            if block[(bit + 2) / 8] & (1 << ((bit + 2) % 8)) != 0 {
                rest[bit / 8] |= 1 << (bit % 8);
            }
        }
        let start = s.steps.len();
        frame_script(
            &mut s,
            &[0x93, 0x22, uid[0] & 3],
            18,
            &rest,
            false,
            true,
            None,
        );
        for step in s.steps.iter_mut().skip(start) {
            if step.prefix == [0x5e] && step.reply == Some(vec![5, 0]) {
                step.reply = Some(vec![5, 12]);
            }
        }
        let mut select = vec![0x93, 0x70];
        select.extend_from_slice(&block);
        frame_script(&mut s, &select, 56, &[0, 0, 0], true, false, None);
        let mut r = reader(s);
        let result = if target {
            r.select(&TagId::NfcA(uid.to_vec()), Duration::from_secs(1))
                .unwrap()
        } else {
            r.discover(Technology::NfcA, Duration::from_secs(1))
                .unwrap()
                .unwrap()
        };
        assert_eq!(result.id(), TagId::NfcA(uid.to_vec()));
        finish(&mut r);
    }
}
#[test]
fn cascade_marker_and_sak_must_agree() {
    for (block, sak) in [([0x88, 1, 2, 3, 0x88], 0), ([1, 2, 3, 4, 4], 4)] {
        let mut s = Script::default();
        frame_script(&mut s, &[], 0, &[4, 0], false, false, Some(0xc6));
        frame_script(&mut s, &[0x93, 0x20], 16, &block, false, true, None);
        let mut select = vec![0x93, 0x70];
        select.extend_from_slice(&block);
        frame_script(&mut s, &select, 56, &[sak, 0, 0], true, false, None);
        off(&mut s);
        let mut r = reader(s);
        assert!(matches!(
            r.discover(Technology::NfcA, Duration::from_secs(1))
                .unwrap_err()
                .kind,
            ErrorKind::InvalidResponse("UID cascade marker/SAK")
        ));
        finish(&mut r);
    }
}
#[test]
fn b_crc_and_invalid_attrib_are_rejected() {
    for crc in [true, false] {
        let mut s = Script::default();
        let mut atqb = [0; 14];
        atqb[0] = 0x50;
        atqb[10] = 1;
        frame_script(&mut s, &[5, 0, 0], 24, &atqb, true, false, None);
        if crc {
            for step in &mut s.steps {
                if step.reply == Some(vec![0x18, 0, 0, 0]) {
                    step.reply = Some(vec![0x18, 0, 0x80, 0]);
                }
            }
            // Remove FIFO reads and stop-NRT: CRC is detected before consuming FIFO.
            s.steps.truncate(s.steps.len() - 3);
        } else {
            frame_script(
                &mut s,
                &[0x1d, 0, 0, 0, 0, 0, 8, 1, 0],
                72,
                &[1, 0, 0],
                true,
                false,
                None,
            );
        }
        off(&mut s);
        let mut r = reader(s);
        r.technology = Some(Technology::NfcB);
        let e = r
            .discover(Technology::NfcB, Duration::from_secs(1))
            .unwrap_err();
        if crc {
            assert!(matches!(e.kind, ErrorKind::Crc));
        } else {
            assert!(matches!(
                e.kind,
                ErrorKind::InvalidResponse("ATTRIB CID/length")
            ));
        }
        finish(&mut r);
    }
}
#[test]
fn tb_wrong_uid_is_a_selection_failure() {
    let mut s = Script::default();
    tb_inventory_reset_script(&mut s);
    frame_script(&mut s, &[6, 0], 16, &[7, 0, 0], true, false, None);
    frame_script(&mut s, &[0x0e, 7], 16, &[7, 0, 0], true, false, None);
    frame_script(&mut s, &[0x0b], 8, &[0; 10], true, false, None);
    off(&mut s);
    let mut r = reader(s);
    r.technology = Some(Technology::St25tb);
    assert!(matches!(
        r.select(&TagId::St25tb([1; 8]), Duration::from_secs(1))
            .unwrap_err()
            .kind,
        ErrorKind::TagMismatch
    ));
    finish(&mut r);
}
#[test]
fn ambiguous_tb_transmit_holds_field_after_blocking_ioctl() {
    let mut s = Script::default();
    frame_script(&mut s, &[9, 7, 1, 2, 3, 4], 48, &[], true, false, None);
    let tx = s
        .steps
        .iter()
        .position(|step| step.prefix == [0xc4])
        .unwrap();
    s.steps.truncate(tx);
    s.fail(&[0xc4], false, 5);
    off(&mut s);
    s.tx_delay = Duration::from_millis(20);
    let mut r = reader(s);
    r.technology = Some(Technology::St25tb);
    let began = Instant::now();
    let e = r
        .exchange(
            &[9, 7, 1, 2, 3, 4],
            &mut [],
            FrameOptions {
                tx_only: true,
                field_hold: Duration::ZERO,
                ..FrameOptions::default()
            },
            Duration::from_millis(10),
        )
        .unwrap_err();
    assert_eq!(e.raw_os_error(), Some(5));
    assert_eq!(e.transmission, Transmission::PossiblyStarted);
    assert!(began.elapsed() >= Duration::from_millis(30));
    assert!(r.is_poisoned());
    finish(&mut r);
}
#[test]
fn tx_only_waits_for_eof_and_programming() {
    let mut s = Script::default();
    frame_script(&mut s, &[9, 7, 1, 2, 3, 4], 48, &[], true, false, None);
    let tx = s
        .steps
        .iter()
        .position(|step| step.prefix == [0xc4])
        .unwrap();
    s.steps.truncate(tx + 3);
    s.write(&[0xc2]);
    let mut r = reader(s);
    r.technology = Some(Technology::St25tb);
    let began = Instant::now();
    let f = r
        .exchange(
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
    assert_eq!(f.outcome, Outcome::Transmitted);
    assert!(began.elapsed() >= Duration::from_millis(10));
    finish(&mut r);
}
#[test]
fn receive_only_uses_unmask_and_nrt() {
    let mut s = Script::default();
    frame_script(&mut s, &[], 0, &[0xaa, 0, 0], true, false, Some(0xc4));
    let tx = s
        .steps
        .iter()
        .position(|step| step.prefix == [0xc4])
        .unwrap();
    s.steps.remove(tx);
    s.steps.remove(tx);
    s.steps.remove(tx);
    s.steps.insert(
        tx,
        Step {
            prefix: vec![0xd1],
            reply: None,
            count: 1,
            errno: None,
        },
    );
    s.steps.insert(
        tx + 1,
        Step {
            prefix: vec![0xe3],
            reply: None,
            count: 1,
            errno: None,
        },
    );
    s.steps[tx + 2].reply = Some(vec![0x10, 0, 0, 0]);
    let mut r = reader(s);
    let f = r
        .exchange(
            &[],
            &mut [0; 3],
            FrameOptions::default(),
            Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(f.bytes, 3);
    finish(&mut r);
}

mod regressions;

fn tb_inventory_reset_script(s: &mut Script) {
    let start = s.steps.len();
    frame_script(s, &[0x0c], 8, &[], true, false, None);
    let tx = s
        .steps
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, step)| step.prefix == [0xc4])
        .unwrap()
        .0;
    s.steps.truncate(tx + 3);
    s.write(&[0xc2]);
}
