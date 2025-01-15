use embedded_hal::delay::DelayNs;
use hal::{Delay, I2cdev};
use linux_embedded_hal as hal;
use sgp30::FeatureSet;
use sgp30::Measurement;
use sgp30::Sgp30;

fn main() {
    println!("Hello, world!");

    let dev = I2cdev::new("/dev/i2c-1").unwrap();
    let address = 0x58;
    let mut sgp = Sgp30::new(dev, address, Delay);

    let serial_number: [u8; 6] = sgp.serial().unwrap();
    println!("serial number: {:?}", serial_number);
    let feature_set: FeatureSet = sgp.get_feature_set().unwrap();
    println!("feature set: {:?}", feature_set);

    sgp.init().unwrap();
    loop {
        let measurement: Measurement = sgp.measure().unwrap();
        println!("CO₂eq parts per million: {}", measurement.co2eq_ppm);
        println!("TVOC parts per billion: {}", measurement.tvoc_ppb);
        Delay.delay_ms(1000 - 12);
    }
}
