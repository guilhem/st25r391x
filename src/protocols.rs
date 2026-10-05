use super::*;

impl<D: Device> St25r391x<D> {
    /// Discover and activate one tag. NFC-A collisions follow the zero branch;
    /// B and ST25TB use one slot and report collisions, rather than enumerate.
    /// A successful operation leaves the field on. All exchanges share one deadline.
    pub fn discover(&mut self, technology: Technology, timeout: Duration) -> Result<Option<Tag>> {
        let deadline = self.start(timeout)?;
        supported(technology).map_err(|k| self.error(k))?;
        let result = self.discover_inner(technology, None, deadline);
        match result {
            Ok(t) => Ok(t),
            Err(e) => self.failure(e),
        }
    }
    /// Reactivate a tag and verify its UID/PUPI. Random NFC-B PUPIs may change
    /// across RF resets; UID byte order is the order returned over the air.
    pub fn select(&mut self, id: &TagId, timeout: Duration) -> Result<Tag> {
        let deadline = self.start(timeout)?;
        let technology = match id {
            TagId::NfcA(uid) => {
                if ![4, 7, 10].contains(&uid.len()) {
                    return Err(
                        self.error(ErrorKind::InvalidArgument("UID length must be 4, 7 or 10"))
                    );
                }
                Technology::NfcA
            }
            TagId::NfcB(_) => Technology::NfcB,
            TagId::St25tb(_) => Technology::St25tb,
        };
        let result = self
            .discover_inner(technology, Some(id), deadline)
            .and_then(|t| t.ok_or_else(|| self.error(ErrorKind::NoResponse)))
            .and_then(|t| {
                if t.id() == *id {
                    Ok(t)
                } else {
                    Err(self.error(ErrorKind::TagMismatch))
                }
            });
        match result {
            Ok(t) => Ok(t),
            Err(e) => self.failure(e),
        }
    }
    fn discover_inner(
        &mut self,
        tech: Technology,
        target: Option<&TagId>,
        deadline: Instant,
    ) -> Result<Option<Tag>> {
        self.field_on_inner(tech, deadline)?;
        match tech {
            Technology::NfcA => self.select_a(target, deadline),
            Technology::NfcB => self.select_b(target, deadline),
            Technology::St25tb => self.select_tb(deadline),
            _ => Err(self.error(ErrorKind::Unsupported(tech))),
        }
    }
    fn protocol_frame(
        &mut self,
        tx: &[u8],
        rx: &mut [u8],
        deadline: Instant,
    ) -> Result<FrameResult> {
        self.frame(
            tx,
            tx.len() * 8,
            rx,
            FrameOptions {
                allow_no_response: true,
                ..FrameOptions::default()
            },
            None,
            false,
            deadline,
        )
    }
    fn select_a(&mut self, target: Option<&TagId>, deadline: Instant) -> Result<Option<Tag>> {
        let mut rx = [0; FIFO_CAPACITY];
        let opt = FrameOptions {
            tx_crc: false,
            rx_crc: false,
            allow_no_response: true,
            ..FrameOptions::default()
        };
        let req = self.frame(
            &[],
            0,
            &mut rx,
            opt,
            Some(if target.is_some() { 0xc7 } else { 0xc6 }),
            false,
            deadline,
        )?;
        if req.outcome == Outcome::NoResponse {
            return Ok(None);
        }
        if req.bits != 16 {
            return Err(self.error(ErrorKind::InvalidResponse("ATQA length")));
        }
        let atqa = [rx[0], rx[1]];
        let wanted = match target {
            Some(TagId::NfcA(uid)) => Some(uid.as_slice()),
            _ => None,
        };
        let mut uid = Vec::new();
        let mut sak = 0;
        for level in 0..3 {
            self.check(deadline)?;
            let sel = [0x93, 0x95, 0x97][level];
            let mut block = [0u8; 5];
            let desired = wanted.map(|u| cascade_block(u, level));
            let mut known = 0usize;
            // At most forty collision positions, always advancing by a bit.
            for _ in 0..41 {
                let mut tx = [0; 7];
                tx[0] = sel;
                tx[1] = (((2 + known / 8) as u8) << 4) | (known % 8) as u8;
                tx[2..].copy_from_slice(&block);
                let r = self.frame(&tx, 16 + known, &mut rx, opt, None, true, deadline)?;
                if r.outcome == Outcome::NoResponse {
                    return Err(self.error(ErrorKind::NoResponse));
                }
                let collision = self.irqs[0] & 4 != 0;
                let received = if collision {
                    let position = self.reg(0x20, deadline)?;
                    if position & 1 != 0 {
                        return Err(self.error(ErrorKind::Parity));
                    }
                    usize::from(position >> 4) * 8 + usize::from((position >> 1) & 7)
                } else {
                    r.bits
                };
                if received > r.bits || known + received > 40 {
                    return Err(self.error(ErrorKind::InvalidResponse("anticollision position")));
                }
                for bit in 0..received {
                    put_bit(&mut block, known + bit, get_bit(&rx, bit));
                }
                known += received;
                if collision {
                    if known >= 32 {
                        return Err(self.error(ErrorKind::InvalidResponse("collision in BCC")));
                    }
                    let choice = desired.as_ref().is_some_and(|d| get_bit(d, known));
                    put_bit(&mut block, known, choice);
                    known += 1;
                } else {
                    if known != 40 {
                        return Err(
                            self.error(ErrorKind::InvalidResponse("anticollision UID length"))
                        );
                    }
                    break;
                }
            }
            if known != 40 || block[4] != block[..4].iter().fold(0, |a, b| a ^ b) {
                return Err(self.error(ErrorKind::InvalidResponse("UID BCC")));
            }
            if let Some(expected) = desired {
                if expected != block {
                    return Err(self.error(ErrorKind::TagMismatch));
                }
            }
            let mut tx = [sel, 0x70, 0, 0, 0, 0, 0];
            tx[2..].copy_from_slice(&block);
            let r = self.protocol_frame(&tx, &mut rx, deadline)?;
            if r.bits != 24 {
                return Err(self.error(ErrorKind::InvalidResponse("SAK length")));
            }
            sak = rx[0];
            let cascade = sak & 4 != 0;
            if cascade != (block[0] == 0x88) || cascade && level == 2 {
                return Err(self.error(ErrorKind::InvalidResponse("UID cascade marker/SAK")));
            }
            uid.extend_from_slice(if cascade { &block[1..4] } else { &block[..4] });
            if !cascade {
                break;
            }
        }
        let mut ats = Vec::new();
        if sak & 0x20 != 0 {
            let r = self.protocol_frame(&[0xe0, 0x80], &mut rx, deadline)?;
            if r.bytes < 4 || usize::from(rx[0]) + 2 != r.bytes {
                return Err(self.error(ErrorKind::InvalidResponse("ATS TL")));
            }
            ats.extend_from_slice(&rx[..r.bytes - 2]);
        }
        Ok(Some(Tag::NfcA(NfcATag {
            uid,
            atqa,
            sak,
            ats,
        })))
    }
    fn select_b(&mut self, target: Option<&TagId>, deadline: Instant) -> Result<Option<Tag>> {
        let mut rx = [0; FIFO_CAPACITY];
        let r = self.protocol_frame(
            &[0x05, 0, if target.is_some() { 8 } else { 0 }],
            &mut rx,
            deadline,
        )?;
        if r.outcome == Outcome::NoResponse {
            return Ok(None);
        }
        if r.bytes != 14 || rx[0] != 0x50 {
            return Err(self.error(ErrorKind::InvalidResponse("ATQB")));
        }
        let pupi: [u8; 4] = rx[1..5].try_into().unwrap();
        if let Some(TagId::NfcB(id)) = target {
            if *id != pupi {
                return Err(self.error(ErrorKind::TagMismatch));
            }
        }
        let application_data = rx[5..9].try_into().unwrap();
        let protocol_info: [u8; 3] = rx[9..12].try_into().unwrap();
        if protocol_info[1] & 1 == 0 {
            return Err(self.error(ErrorKind::InvalidResponse("ATQB does not support ISO-DEP")));
        }
        let mut attrib = [0x1d, 0, 0, 0, 0, 0, 0x08, 0x01, 0];
        attrib[1..5].copy_from_slice(&pupi);
        let r = self.protocol_frame(&attrib, &mut rx, deadline)?;
        if r.bytes != 3 || rx[0] & 15 != 0 {
            return Err(self.error(ErrorKind::InvalidResponse("ATTRIB CID/length")));
        }
        Ok(Some(Tag::NfcB(NfcBTag {
            pupi,
            application_data,
            protocol_info,
            cid: 0,
            mbli: rx[0] >> 4,
        })))
    }
    fn select_tb(&mut self, deadline: Instant) -> Result<Option<Tag>> {
        let mut rx = [0; FIFO_CAPACITY];
        let r = self.protocol_frame(&[0x06, 0], &mut rx, deadline)?;
        if r.outcome == Outcome::NoResponse {
            return Ok(None);
        }
        if r.bytes != 3 {
            return Err(self.error(ErrorKind::InvalidResponse("ST25TB INITIATE")));
        }
        let chip_id = rx[0];
        let r = self.protocol_frame(&[0x0e, chip_id], &mut rx, deadline)?;
        if r.bytes != 3 || rx[0] != chip_id {
            return Err(self.error(ErrorKind::InvalidResponse("ST25TB SELECT")));
        }
        let r = self.protocol_frame(&[0x0b], &mut rx, deadline)?;
        if r.bytes != 10 {
            return Err(self.error(ErrorKind::InvalidResponse("ST25TB UID length")));
        }
        Ok(Some(Tag::St25tb(St25tbTag {
            uid: rx[..8].try_into().unwrap(),
            chip_id,
        })))
    }
}
fn get_bit(bytes: &[u8], bit: usize) -> bool {
    bytes[bit / 8] & (1 << (bit % 8)) != 0
}
fn put_bit(bytes: &mut [u8], bit: usize, value: bool) {
    if value {
        bytes[bit / 8] |= 1 << (bit % 8);
    } else {
        bytes[bit / 8] &= !(1 << (bit % 8));
    }
}
fn cascade_block(uid: &[u8], level: usize) -> [u8; 5] {
    let mut block = [0; 5];
    let offset = level * 3;
    if uid.len() > offset + 4 {
        block[0] = 0x88;
        block[1..4].copy_from_slice(&uid[offset..offset + 3]);
    } else if uid.len() >= offset + 4 {
        block[..4].copy_from_slice(&uid[offset..offset + 4]);
    }
    block[4] = block[..4].iter().fold(0, |a, b| a ^ b);
    block
}
