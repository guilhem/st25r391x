use i2cdev::linux::LinuxI2CDevice;
use st25r391x::{Settings, St25r391x, Technology, I2C_ADDRESS};
use std::time::Duration;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Check the board's actual VDD first; supply_3v=true means 2.4..=3.6 V.
    let device = LinuxI2CDevice::new("/dev/i2c-1", I2C_ADDRESS)?;
    let mut reader = St25r391x::new(device);
    println!(
        "Identity: {:?}",
        reader.initialize(Settings::default(), Duration::from_secs(1))?
    );
    let result = (|| {
        for technology in [Technology::NfcA, Technology::NfcB, Technology::St25tb] {
            println!(
                "{technology:?}: {:?}",
                reader.discover(technology, Duration::from_secs(1))?
            );
        }
        Ok::<_, st25r391x::Error>(())
    })();
    let shutdown = reader.shutdown(Duration::from_millis(100));
    result?;
    shutdown?;
    Ok(())
}
