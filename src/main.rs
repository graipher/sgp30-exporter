use std::env;
use std::error::Error;
use std::time::{Duration, SystemTime};

use hal::{Delay, I2cdev};
use linux_embedded_hal as hal;
use prometheus_exporter::prometheus::{register_gauge, register_gauge_vec, Gauge};
use prometheus_parse::{Scrape, Value};
use sgp30::{Humidity, Measurement, Sgp30};
use tokio::{signal, time::sleep};

const DEFAULT_PORT: &str = "9185";
const DEFAULT_HUMIDITY_URL: &str = "http://raspberrypi5:9521/metrics";
const DEFAULT_HUMIDITY_MAC: &str = "e9:60:94:11:db:5e";
const I2C_DEVICE: &str = "/dev/i2c-1";
const SGP30_ADDRESS: u8 = 0x58;
const TEMPERATURE_METRIC: &str = "ruuvi_temperature_celsius";
const HUMIDITY_METRIC: &str = "ruuvi_humidity_ratio";

/// Calculate vapor pressure (in hPa) from temperature (in °C).
fn vapor_pressure(t: f64) -> f64 {
    6.112 * (17.67 * t / (243.5 + t)).exp()
}

/// Calculate absolute humidity (in g/m³) from temperature (in °C) and relative humidity (%).
fn absolute_humidity(t: f64, rh: f64) -> f64 {
    vapor_pressure(t) * rh * 2.1674 / (273.15 + t)
}

/// Fetch and parse temperature and humidity metrics from the given URL.
async fn fetch_humidity_metrics(
    url: &str,
    target_device: &str,
) -> Result<(f64, f64), Box<dyn Error>> {
    let body = reqwest::get(url).await?.text().await?;
    let metrics = Scrape::parse(body.lines().map(|s| Ok(s.to_owned())).into_iter())?;
    let mut temperature = None;
    let mut humidity = None;

    for sample in metrics.samples {
        if let Some(device) = sample.labels.get("device") {
            if device == target_device {
                match sample.metric.as_str() {
                    TEMPERATURE_METRIC => {
                        if let Value::Gauge(v) = sample.value {
                            temperature = Some(v);
                        }
                    }
                    HUMIDITY_METRIC => {
                        if let Value::Gauge(v) = sample.value {
                            humidity = Some(v * 100.0); // Convert ratio to percentage
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    match (temperature, humidity) {
        (Some(t), Some(h)) => Ok((t, h)),
        _ => Err("Failed to fetch temperature or humidity".into()),
    }
}

/// Initialize the SGP30 sensor and return its instance.
fn initialize_sgp30() -> Result<Sgp30<I2cdev, Delay>, Box<dyn Error>> {
    let dev = I2cdev::new(I2C_DEVICE)?;
    let mut sgp = Sgp30::new(dev, SGP30_ADDRESS, Delay);

    sgp.init().unwrap();
    let serial_number = sgp.serial().unwrap();
    let feature_set = sgp.get_feature_set().unwrap();

    println!("SGP30 initialized with serial number: {:?}", serial_number);
    println!("Feature set: {:?}", feature_set);

    Ok(sgp)
}

/// Update Prometheus metrics.
fn update_metrics(tvoc: &Gauge, co2eq: &Gauge, last_updated: &Gauge, measurement: &Measurement) {
    tvoc.set(measurement.tvoc_ppb as f64);
    co2eq.set(measurement.co2eq_ppm as f64);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    last_updated.set(now as f64);

    println!(
        "Updated metrics - CO₂eq: {} ppm, TVOC: {} ppb",
        measurement.co2eq_ppm, measurement.tvoc_ppb
    );
}

/// Main loop to fetch humidity metrics and update the SGP30 sensor.
async fn main_loop(
    sgp: &mut Sgp30<I2cdev, Delay>,
    tvoc: &Gauge,
    co2eq: &Gauge,
    last_updated: &Gauge,
    url: &str,
    target_device: &str,
) -> Result<(), Box<dyn Error>> {
    loop {
        match fetch_humidity_metrics(url, target_device).await {
            Ok((temperature, relative_humidity)) => {
                println!(
                    "Fetched metrics - Temperature: {:.2} °C, Humidity: {:.2} %",
                    temperature, relative_humidity
                );

                let abs_humidity = absolute_humidity(temperature, relative_humidity);
                if let Ok(h_abs) = Humidity::from_f32(abs_humidity as f32) {
                    if let Err(e) = sgp.set_humidity(Some(&h_abs)) {
                        eprintln!("Failed to set humidity: {:?}", e);
                    } else {
                        println!("Set absolute humidity to {:.2} g/m³", abs_humidity);
                    }
                }

                match sgp.measure() {
                    Ok(measurement) => update_metrics(tvoc, co2eq, last_updated, &measurement),
                    Err(e) => eprintln!("Measurement failed: {:?}", e),
                }
            }
            Err(e) => eprintln!("Failed to fetch humidity metrics: {:?}", e),
        }

        sleep(Duration::from_secs(5)).await;
    }
}

/// Handle graceful shutdown.
async fn shutdown_signal() {
    signal::ctrl_c()
        .await
        .expect("Failed to install Ctrl+C handler");
    println!("Shutdown signal received. Exiting...");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let port = env::var("PORT").unwrap_or_else(|_| DEFAULT_PORT.to_string());
    let binding = format!("0.0.0.0:{}", port).parse()?;
    let _exporter = prometheus_exporter::start(binding)?;

    let last_updated = register_gauge!("sgp30_last_updated", "Last update timestamp")?;
    let process_start_time = register_gauge!(
        "process_start_time_seconds",
        "Process start time in seconds"
    )?;
    let tvoc = register_gauge!("sgp30_tvoc", "TVOC in ppb")?;
    let co2eq = register_gauge!("sgp30_co2eq", "CO₂eq in ppm")?;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    process_start_time.set(now as f64);

    let compile_datetime = compile_time::datetime_str!();
    let rustc_version = compile_time::rustc_version_str!();
    let rust_info = register_gauge_vec!(
        "rust_info",
        "Info about the Rust version",
        &["rustc_version", "compile_time"]
    )
    .unwrap();
    rust_info
        .get_metric_with_label_values(&[rustc_version, compile_datetime])
        .unwrap()
        .set(1.);

    println!("Exporter listening on port: {}", port);

    let url = env::var("HUMIDITY_URL").unwrap_or_else(|_| DEFAULT_HUMIDITY_URL.to_string());
    let target_device =
        env::var("HUMIDITY_MAC").unwrap_or_else(|_| DEFAULT_HUMIDITY_MAC.to_string());

    let mut sgp = initialize_sgp30()?;

    tokio::select! {
        _ = main_loop(&mut sgp, &tvoc, &co2eq, &last_updated, &url, &target_device) => {},
        _ = shutdown_signal() => {},
    }
    Ok(())
}
