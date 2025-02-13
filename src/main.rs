use std::env;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::time::{Duration, SystemTime};

use hal::{Delay, I2cdev};
use linux_embedded_hal as hal;
use prometheus_exporter::prometheus::{
    register_counter, register_counter_vec, register_gauge, register_gauge_vec, register_histogram,
    Counter, CounterVec, Gauge, GaugeVec, Histogram,
};
use prometheus_parse::{Scrape, Value};
use sgp30::{Baseline, Humidity, Measurement, Sgp30};
use sysinfo::{
    Components, DiskRefreshKind, Disks, MemoryRefreshKind, Networks, ProcessRefreshKind,
    ProcessesToUpdate, System,
};
use tokio::signal;
use tokio::time::{sleep_until, Instant};

const DEFAULT_PORT: &str = "9185";
const DEFAULT_HUMIDITY_URL: &str = "http://raspberrypi5:9521/metrics";
const DEFAULT_HUMIDITY_MAC: &str = "e9:60:94:11:db:5e";
const I2C_DEVICE: &str = "/dev/i2c-1";
const SGP30_ADDRESS: u8 = 0x58;
const TEMPERATURE_METRIC: &str = "ruuvi_temperature_celsius";
const HUMIDITY_METRIC: &str = "ruuvi_humidity_ratio";
const BASELINE_FILE: &str = "sgp30_baseline.dat";

/// Load the baseline from a file if available.
fn load_baseline() -> Option<Baseline> {
    let mut file = File::open(BASELINE_FILE).ok()?;
    let mut content = String::new();
    file.read_to_string(&mut content).ok()?;

    let mut parts = content.split_whitespace();
    let co2eq = parts.next()?.parse().ok()?;
    let tvoc = parts.next()?.parse().ok()?;

    Some(Baseline { co2eq, tvoc })
}

/// Save the baseline to a file.
fn save_baseline(baseline: &Baseline) {
    if let Ok(mut file) = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(BASELINE_FILE)
    {
        let _ = writeln!(file, "{} {}", baseline.co2eq, baseline.tvoc);
    }
}

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
async fn initialize_sgp30() -> Result<Sgp30<I2cdev, Delay>, Box<dyn Error>> {
    let dev = I2cdev::new(I2C_DEVICE)?;
    let mut sgp = Sgp30::new(dev, SGP30_ADDRESS, Delay);

    sgp.init().unwrap();
    let serial_number = sgp.serial().unwrap();
    let feature_set = sgp.get_feature_set().unwrap();

    println!("Initializing SGP30 with serial number: {:?}", serial_number);
    println!("Feature set: {:?}", feature_set);

    if let Some(baseline) = load_baseline() {
        if let Err(e) = sgp.set_baseline(&baseline) {
            eprintln!("Failed to restore baseline: {:?}", e);
        } else {
            println!(
                "Restored baseline - CO₂eq: {}, TVOC: {}",
                baseline.co2eq, baseline.tvoc
            );
        }
    }

    let mut i: u8 = 0;
    loop {
        if i == 15 {
            println!("");
            break;
        }
        let sleep_target = Instant::now() + Duration::from_secs(1);
        match sgp.measure() {
            Ok(measurement) => {
                if measurement.co2eq_ppm != 400 || measurement.tvoc_ppb != 0 {
                    println!("");
                    break;
                } else {
                    print!(".");
                    io::stdout().flush().unwrap();
                }
            }
            Err(e) => eprintln!("Measurement failed: {:?}", e),
        }
        i = i + 1;
        sleep_until(sleep_target).await;
    }

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
        "{}: Updated metrics - CO₂eq: {} ppm, TVOC: {} ppb",
        now, measurement.co2eq_ppm, measurement.tvoc_ppb
    );
}

/// Main loop to fetch humidity metrics and update the SGP30 sensor.
async fn main_loop(
    sgp: &mut Sgp30<I2cdev, Delay>,
    tvoc: &Gauge,
    co2eq: &Gauge,
    last_updated: &Gauge,
    process_cpu_seconds: &Counter,
    process_resident_memory_bytes: &Gauge,
    sysinfo_temperature: &GaugeVec,
    sysinfo_cpu_usage: &GaugeVec,
    sysinfo_memory_total_bytes: &Gauge,
    sysinfo_memory_used_bytes: &Gauge,
    sysinfo_network_bytes_sent: &CounterVec,
    sysinfo_network_bytes_received: &CounterVec,
    sysinfo_disk_total_bytes: &GaugeVec,
    sysinfo_disk_available_bytes: &GaugeVec,
    sysinfo_disk_read_bytes: &CounterVec,
    sysinfo_disk_write_bytes: &CounterVec,
    loop_duration: &Histogram,
    url: &str,
    target_device: &str,
) -> Result<(), Box<dyn Error>> {
    let mut sys = System::new();
    let mut components = Components::new_with_refreshed_list();
    let mut networks = Networks::new_with_refreshed_list();
    let mut disks = Disks::new_with_refreshed_list();
    let pid = sysinfo::get_current_pid().expect("Failed to get PID");

    let mut last_time = Instant::now();
    let mut sleep_target = Instant::now();
    let mut i: u16 = 0;

    loop {
        sleep_target = sleep_target + Duration::from_secs(1);
        let timer = loop_duration.start_timer();

        // update system metrics
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        if let Some(process) = sys.process(pid) {
            let now = Instant::now();
            let elapsed = now.duration_since(last_time).as_secs_f64();
            let cpu_usage = (process.cpu_usage() / 100.0) as f64; // Convert percentage to fraction
            process_cpu_seconds.inc_by(cpu_usage * elapsed);
            process_resident_memory_bytes.set(process.memory() as f64);
            last_time = now;
        }
        components.refresh(true);
        for component in &components {
            if let Some(temperature) = component.temperature() {
                sysinfo_temperature
                    .with_label_values(&[component.label()])
                    .set(temperature as f64);
            }
        }
        sys.refresh_cpu_usage();
        for cpu in sys.cpus() {
            sysinfo_cpu_usage
                .with_label_values(&[cpu.name()])
                .set(cpu.cpu_usage() as f64);
        }
        sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        sysinfo_memory_total_bytes.set(sys.total_memory() as f64);
        sysinfo_memory_used_bytes.set(sys.used_memory() as f64);
        networks.refresh(true);
        for (interface_name, data) in &networks {
            sysinfo_network_bytes_sent
                .with_label_values(&[interface_name])
                .inc_by(data.transmitted() as f64);
            sysinfo_network_bytes_received
                .with_label_values(&[interface_name])
                .inc_by(data.received() as f64);
        }
        disks.refresh_specifics(true, DiskRefreshKind::everything());
        for disk in &disks {
            let disk_name = disk.name().to_str().unwrap_or("unknown");
            sysinfo_disk_total_bytes
                .with_label_values(&[disk_name])
                .set(disk.total_space() as f64);
            sysinfo_disk_available_bytes
                .with_label_values(&[disk_name])
                .set(disk.available_space() as f64);
            let usage = disk.usage();
            sysinfo_disk_read_bytes
                .with_label_values(&[disk_name])
                .inc_by(usage.read_bytes as f64);
            sysinfo_disk_write_bytes
                .with_label_values(&[disk_name])
                .inc_by(usage.written_bytes as f64);
        }

        if (i % 60) == 0 {
            match fetch_humidity_metrics(url, target_device).await {
                Ok((temperature, relative_humidity)) => {
                    let now = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    let abs_humidity = absolute_humidity(temperature, relative_humidity);
                    if let Ok(h_abs) = Humidity::from_f32(abs_humidity as f32) {
                        if let Err(e) = sgp.set_humidity(Some(&h_abs)) {
                            eprintln!("Failed to set humidity: {:?}", e);
                        } else {
                            println!(
                                "{}: Fetched metrics - Temperature: {:.2} °C, Humidity: {:.2} % / {:.2} g/m³",
                                now, temperature, relative_humidity, abs_humidity
                            );
                        }
                    }
                }
                Err(e) => eprintln!("Failed to fetch humidity metrics: {:?}", e),
            }
        }

        match sgp.measure() {
            Ok(measurement) => update_metrics(tvoc, co2eq, last_updated, &measurement),
            Err(e) => eprintln!("Measurement failed: {:?}", e),
        }

        // Save baseline every 10 minutes
        if i % 600 == 599 {
            match sgp.get_baseline() {
                Ok(baseline) => {
                    save_baseline(&baseline);
                    println!(
                        "Saved baseline - CO₂eq: {}, TVOC: {}",
                        baseline.co2eq, baseline.tvoc
                    );
                }
                Err(e) => eprintln!("Failed to get baseline: {:?}", e),
            }
        }

        i = (i + 1) % 600;
        timer.stop_and_record();
        sleep_until(sleep_target).await;
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
    let process_start_time = register_counter!(
        "process_start_time_seconds",
        "Process start time in seconds"
    )?;
    let process_cpu_seconds_total = register_counter!(
        "process_cpu_seconds_total",
        "Total CPU seconds consumed by the process"
    )?;
    let process_resident_memory_bytes = register_gauge!(
        "process_resident_memory_bytes",
        "Size of resident memory set in bytes"
    )?;
    let sysinfo_temperature =
        register_gauge_vec!("sysinfo_temperature", "Temperature in °C", &["label"])?;
    let sysinfo_cpu_usage =
        register_gauge_vec!("sysinfo_cpu_usage", "CPU usage in percentage", &["name"])?;
    let sysinfo_memory_total_bytes =
        register_gauge!("sysinfo_memory_total_bytes", "Total memory in bytes")?;
    let sysinfo_memory_used_bytes =
        register_gauge!("sysinfo_memory_used_bytes", "Used memory in bytes")?;
    let sysinfo_network_bytes_sent =
        register_counter_vec!("sysinfo_network_bytes_sent", "Bytes sent", &["interface"])?;
    let sysinfo_network_bytes_received = register_counter_vec!(
        "sysinfo_network_bytes_received",
        "Bytes received",
        &["interface"]
    )?;
    let sysinfo_disk_read_bytes =
        register_counter_vec!("sysinfo_disk_read_bytes", "Bytes read", &["disk"])?;
    let sysinfo_disk_write_bytes =
        register_counter_vec!("sysinfo_disk_write_bytes", "Bytes written", &["disk"])?;
    let sysinfo_disk_total_bytes =
        register_gauge_vec!("sysinfo_disk_total_bytes", "Total disk space", &["disk"])?;
    let sysinfo_disk_available_bytes = register_gauge_vec!(
        "sysinfo_disk_available_bytes",
        "Available disk space",
        &["disk"]
    )?;
    let loop_duration = register_histogram!(
        "loop_duration",
        "duration of SGP30 measurement loop in seconds"
    )?;

    let tvoc = register_gauge!("sgp30_tvoc", "TVOC in ppb")?;
    let co2eq = register_gauge!("sgp30_co2eq", "CO₂eq in ppm")?;
    co2eq.set(400 as f64);

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    process_start_time.inc_by(now as f64);

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

    let mut sgp = initialize_sgp30().await?;

    tokio::select! {
        _ = main_loop(
            &mut sgp,
            &tvoc,
            &co2eq,
            &last_updated,
            &process_cpu_seconds_total,
            &process_resident_memory_bytes,
            &sysinfo_temperature,
            &sysinfo_cpu_usage,
            &sysinfo_memory_total_bytes,
            &sysinfo_memory_used_bytes,
            &sysinfo_network_bytes_sent,
            &sysinfo_network_bytes_received,
            &sysinfo_disk_total_bytes,
            &sysinfo_disk_available_bytes,
            &sysinfo_disk_read_bytes,
            &sysinfo_disk_write_bytes,
            &loop_duration,
            &url,
            &target_device,
        ) => {},
        _ = shutdown_signal() => {},
    }
    Ok(())
}
