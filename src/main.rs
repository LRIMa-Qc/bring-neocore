use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::sleep;

use anyhow::Result;
use glob::glob;
use log::{error, info, warn};
use opencv::{imgcodecs, prelude::*, videoio};
use reqwest::blocking::Client;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, QoS};
use serialport::SerialPort;
use std::collections::HashSet;

// ---------------- CONFIG ----------------
const BAUDRATE: u32 = 9600;
const MQTT_BROKER: &str = "206.167.46.66";
const MQTT_PORT: u16 = 1883;
const MQTT_USERNAME: &str = "dev";
const MQTT_PASSWORD: &str = "lrimalrima";
const MQTT_SUB_TOPIC: &str = "device/write";

const CAMERA_INDEX: i32 = 0;
const CAMERA_WIDTH: i32 = 640;
const CAMERA_HEIGHT: i32 = 480;
const CAMERA_FPS: f64 = 10.0;
const CAMERA_URL: &str = "http://206.167.46.66:3000/camera/frame";

// ---------------- SERIAL BRIDGE ----------------
async fn serial_connect(shared_port: Arc<Mutex<Option<Box<dyn SerialPort>>>>) {
    loop {
        if shared_port.lock().unwrap().is_none() {
            for entry in glob("/dev/ttyACM*").unwrap() {
                if let Ok(path) = entry {
                    match serialport::new(path.to_str().unwrap(), BAUDRATE)
                        .timeout(Duration::from_millis(100))
                        .open()
                    {
                        Ok(port) => {
                            *shared_port.lock().unwrap() = Some(port);
                            info!("Serial connected to {:?}", path);
                            break;
                        }
                        Err(e) => warn!("Failed to open {:?}: {}", path, e),
                    }
                }
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
}

// ---------------- MQTT BRIDGE ----------------
async fn mqtt_task(
    shared_port: Arc<Mutex<Option<Box<dyn SerialPort>>>>,
    tx: mpsc::Sender<(u8, Vec<u8>)>,
) -> Result<()> {
    let mut mqttoptions = MqttOptions::new("rust_bridge", MQTT_BROKER, MQTT_PORT);
    mqttoptions.set_keep_alive(Duration::from_secs(10));
    mqttoptions.set_credentials(MQTT_USERNAME, MQTT_PASSWORD);

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);
    client.subscribe(MQTT_SUB_TOPIC, QoS::AtMostOnce).await?;

    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(incoming)) => {
                if let rumqttc::Packet::Publish(publish) = incoming {
                    let hex_data = String::from_utf8_lossy(&publish.payload);
                    if hex_data.len() % 2 == 0 {
                        if let Ok(bytes) = hex::decode(&hex_data) {
                            // Write to serial
                            if let Some(port) = &mut *shared_port.lock().unwrap() {
                                let _ = port.write(&bytes);
                            }
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => warn!("MQTT error: {}", e),
        }
    }
}

// ---------------- CAMERA STREAMER ----------------
async fn camera_task() -> Result<()> {
    let mut cam = videoio::VideoCapture::new(CAMERA_INDEX, videoio::CAP_V4L2)?;
    cam.set(videoio::CAP_PROP_FRAME_WIDTH, CAMERA_WIDTH as f64)?;
    cam.set(videoio::CAP_PROP_FRAME_HEIGHT, CAMERA_HEIGHT as f64)?;
    cam.set(videoio::CAP_PROP_FPS, CAMERA_FPS)?;

    let client = Client::new();
    let interval = Duration::from_secs_f64(1.0 / CAMERA_FPS);

    loop {
        let mut frame = Mat::default();
        if cam.read(&mut frame)? && !frame.empty()? {
            if let Ok(jpg_bytes) =
                imgcodecs::imencode(".jpg", &frame, &opencv::types::VectorOfint::new())
            {
                let _ = client
                    .post(CAMERA_URL)
                    .header("Content-Type", "image/jpeg")
                    .body(jpg_bytes)
                    .send();
            }
        }
        sleep(interval).await;
    }
}

// ---------------- MAIN LOOP ----------------
#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let shared_port: Arc<Mutex<Option<Box<dyn SerialPort>>>> = Arc::new(Mutex::new(None));

    // Serial connection
    let serial_clone = shared_port.clone();
    tokio::spawn(async move { serial_connect(serial_clone).await });

    // MQTT task
    let (tx, mut rx) = mpsc::channel::<(u8, Vec<u8>)>(100);
    let serial_clone2 = shared_port.clone();
    tokio::spawn(async move { mqtt_task(serial_clone2, tx).await.unwrap() });

    // Camera task
    tokio::spawn(async move { camera_task().await.unwrap() });

    // Verbose logging devices
    let verbose_devices: Arc<Mutex<HashSet<u8>>> = Arc::new(Mutex::new(HashSet::new()));

    // Main serial read loop
    loop {
        if let Some(port) = &mut *shared_port.lock().unwrap() {
            let mut buf = [0u8; 256];
            if let Ok(n) = port.read(&mut buf) {
                if n >= 2 {
                    let dtype = buf[0];
                    let total_size = buf[1] as usize;
                    if total_size >= 2 && n >= total_size {
                        let payload = buf[2..total_size].to_vec();
                        let inverted: Vec<u8> = payload.iter().rev().cloned().collect();
                        let topic = format!("device/{:02X}", dtype);

                        // Here you could send to MQTT
                        // TODO: implement MQTT publishing if needed

                        if verbose_devices.lock().unwrap().contains(&dtype) {
                            info!("Device {:02X} payload={:X?}", dtype, inverted);
                        }
                    }
                }
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
}
