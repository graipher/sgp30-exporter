# SGP30 Air Quality Exporter

Periodically get information from a [SGP30 Air Quality Sensor](https://shop.pimoroni.com/products/sgp30-air-quality-sensor-breakout?variant=30924091719763) connected via I2C and publish it as Prometheus metrics.

## Exposed metrics

Total Volatile Organic Compounds (TVOC) in parts per billion (ppb), Carbon Dioxide equivalent (eCO2) in parts per million (ppm) and last update time.

Can be configured to get the humidity from another exporter for calibration.

# How to run

Build and run with Docker:

```sh
docker build -t sgp30-exporter .
docker run -it --rm \
    -e HUMIDITY_URL=host/ip_address \
    -e HUMIDITY_MAC=mac_address \
    -e PORT=9186 \
    -e PERIOD=60 \
    --device=/dev/i2c-1 \
    sgp30-exporter
```
