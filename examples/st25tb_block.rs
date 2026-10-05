//! Explicit sacrificial-tag qualification; default operation is READ_BLOCK.
use i2cdev::linux::LinuxI2CDevice;
use st25r391x::{FrameOptions, Settings, St25r391x, Tag, Technology, I2C_ADDRESS};
use std::time::Duration;
fn read(
    r: &mut St25r391x<LinuxI2CDevice>,
    block: u8,
) -> Result<[u8; 4], Box<dyn std::error::Error>> {
    let mut rx = [0; 6];
    let f = r.exchange(
        &[8, block],
        &mut rx,
        FrameOptions::default(),
        Duration::from_secs(1),
    )?;
    if f.bytes != 6 {
        return Err("unexpected READ_BLOCK length".into());
    }
    Ok(rx[..4].try_into()?)
}
fn write(
    r: &mut St25r391x<LinuxI2CDevice>,
    block: u8,
    bytes: [u8; 4],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = [9, block, 0, 0, 0, 0];
    tx[2..].copy_from_slice(&bytes);
    r.exchange(
        &tx,
        &mut [],
        FrameOptions {
            tx_only: true,
            ..FrameOptions::default()
        },
        Duration::from_secs(1),
    )?;
    if read(r, block)? != bytes {
        return Err("write verification failed; original data may need restoration".into());
    }
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let write_mode = match args.first().map(String::as_str) {
        None | Some("read") => false,
        Some("--qualify-write-ST25TB512-AT-sacrificial-block7") => true,
        _ => return Err("use read or --qualify-write-ST25TB512-AT-sacrificial-block7".into()),
    };
    let mut r = St25r391x::new(LinuxI2CDevice::new("/dev/i2c-1", I2C_ADDRESS)?);
    r.initialize(Settings::default(), Duration::from_secs(1))?;
    let result = (|| {
        let Some(Tag::St25tb(t)) = r.discover(Technology::St25tb, Duration::from_secs(1))? else {
            return Err("no ST25TB tag".into());
        };
        println!("UID in air order: {:02x?}", t.uid);
        let original = read(&mut r, 7)?;
        println!("Block 7 (LSB byte first): {original:02x?}");
        if write_mode {
            // Owner must identify an unlocked ST25TB512-AT and save this value
            // externally before opting in. UID does not prove tag model.
            let mut probe = original;
            probe[0] ^= 1;
            write(&mut r, 7, probe)?;
            write(&mut r, 7, original)?;
            println!("Pattern verified and original bytes restored and verified");
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })();
    let shutdown = r.shutdown(Duration::from_millis(100));
    result?;
    shutdown?;
    Ok(())
}
