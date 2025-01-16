use std::env;
use std::error::Error;
use std::time::SystemTime;

use embedded_hal::delay::DelayNs;
use hal::{Delay, I2cdev};
use linux_embedded_hal as hal;
use prometheus_exporter::prometheus::register_gauge;
use prometheus_parse;
use sgp30::FeatureSet;
use sgp30::Humidity;
use sgp30::Measurement;
use sgp30::Sgp30;

fn vapor_pressure(t: f64) -> f64 {
    6.112 * (17.67 * t / (243.5 + t)).exp()
}

fn absolute_humidity(t: f64, rh: f64) -> f64 {
    vapor_pressure(t) * rh * 2.1674 / (273.15 + t)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Hello, world!");

    let port = env::var("PORT")
        .or::<String>(Ok("9185".to_string()))
        .unwrap();
    println!("Listening on port :{}", port);
    let binding = format!("0.0.0.0:{}", port).parse().unwrap();
    let _exporter = prometheus_exporter::start(binding).unwrap();

    let last_updated =
        register_gauge!("sgp30_last_updated", "SGP30 last update UNIX timestamp").unwrap();
    let process_start_time =
        register_gauge!("process_start_time_seconds", "Start time of the process").unwrap();

    let tvoc = register_gauge!(
        "sgp30_tvoc",
        "Total volatile organic compounds in parts per billion"
    )
    .unwrap();
    let co2eq = register_gauge!(
        "sgp30_co2eq",
        "Carbon dioxide equivalent in parts per million"
    )
    .unwrap();

    let mut now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    process_start_time.set(now as f64);
    println!("Start time: {}", now);

    let url = env::var("HUMIDITY_URL")
        .or::<String>(Ok("http://raspberrypi5:9521/metrics".to_string()))
        .unwrap();
    let target_device = env::var("HUMIDITY_MAC")
        .or::<String>(Ok("e9:60:94:11:db:5e".to_string()))
        .unwrap();
    let mut t: Option<f64> = None;
    let mut h: Option<f64> = None;

    let dev = I2cdev::new("/dev/i2c-1").unwrap();
    let address = 0x58;
    let mut sgp = Sgp30::new(dev, address, Delay);

    let serial_number: [u8; 6] = sgp.serial().unwrap();
    println!("serial number: {:?}", serial_number);
    let feature_set: FeatureSet = sgp.get_feature_set().unwrap();
    println!("feature set: {:?}", feature_set);

    sgp.init().unwrap();
    loop {
        let body = reqwest::get(url).await?.text().await?;
        let lines: Vec<_> = body.lines().map(|s| Ok(s.to_owned())).collect();
        let metrics = prometheus_parse::Scrape::parse(lines.into_iter())?;
        for sample in metrics.samples.iter() {
            if (sample.metric != "ruuvi_temperature_celsius"
                && sample.metric != "ruuvi_humidity_ratio")
                || sample.labels.get("device").unwrap() != target_device
            {
                continue;
            }
            println!(
                "metric: {}, device: {}, value: {:?}",
                sample.metric,
                sample.labels.get("device").unwrap(),
                match sample.value {
                    prometheus_parse::Value::Gauge(v) => v,
                    _ => panic!("unexpected value type"),
                }
            );
            match sample.metric.as_str() {
                "ruuvi_temperature_celsius" => {
                    t = match sample.value {
                        prometheus_parse::Value::Gauge(v) => Some(v),
                        _ => panic!("unexpected value type"),
                    };
                }
                "ruuvi_humidity_ratio" => {
                    h = match sample.value {
                        prometheus_parse::Value::Gauge(v) => Some(v),
                        _ => panic!("unexpected value type"),
                    };
                }
                _ => panic!("unexpected metric"),
            }
        }
        println!(
            "temperature: {}, relative humidity: {}, absolute humidity: {}",
            t.unwrap(),
            h.unwrap() * 100.0,
            absolute_humidity(t.unwrap(), h.unwrap() * 100.0)
        );
        let h_abs =
            Humidity::from_f32(absolute_humidity(t.unwrap(), h.unwrap() * 100.0) as f32).unwrap();

        match sgp.set_humidity(Some(&h_abs)) {
            Ok(_) => {
                println!("Humidity set to {:?}", h_abs);
            }
            Err(e) => {
                println!("Error setting humidity: {:?}", e);
            }
        }

        let measurement: Measurement = sgp.measure().unwrap();
        co2eq.set(measurement.co2eq_ppm as f64);
        println!("CO₂eq parts per million: {}", measurement.co2eq_ppm);
        tvoc.set(measurement.tvoc_ppb as f64);
        println!("TVOC parts per billion: {}", measurement.tvoc_ppb);
        now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        last_updated.set(now as f64);
        Delay.delay_ms(1000 - 12);
    }
}
