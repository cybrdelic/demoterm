use clap::{Parser, Subcommand};
use ctrlc;
use gif::{Encoder as GifEncoder, Frame as GifFrame, Repeat};
use image::{codecs::png::PngEncoder, Rgb, RgbImage};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use rusttype::{Font, Scale};
use serde::{Deserialize, Serialize};
use std::mem::MaybeUninit;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    process::exit,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

// Constants for file paths
const PID_FILE: &str = "/tmp/demoterm.pid";
const RECORDING_FILE: &str = "/tmp/demoterm_recording.json";
const GIF_FILE: &str = "demoterm.gif";

// Command-line argument definitions
#[derive(Parser)]
#[command(name = "demoterm")]
#[command(about = "Terminal recording and GIF creation tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start terminal recording
    Start,
    /// Stop terminal recording and generate GIF
    Stop,
}

#[derive(Serialize, Deserialize, Debug)]
struct TerminalEvent {
    timestamp: u128, // Milliseconds since start
    input: Option<String>,
    output: Option<String>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Start => {
            if Path::new(PID_FILE).exists() {
                eprintln!("Recording is already in progress.");
                exit(1);
            }

            // Fork the process into a background child
            match unsafe { nix::unistd::fork() } {
                Ok(nix::unistd::ForkResult::Parent { .. }) => {
                    // Parent process exits
                    println!("Recording started.");
                    exit(0);
                }
                Ok(nix::unistd::ForkResult::Child) => {
                    // Child process continues
                    if let Err(e) = run_recorder() {
                        eprintln!("Error in recorder: {}", e);
                        exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Fork failed: {}", e);
                    exit(1);
                }
            }
        }
        Commands::Stop => {
            if !Path::new(PID_FILE).exists() {
                eprintln!("No recording session found.");
                exit(1);
            }

            // Read PID from PID_FILE
            let pid_str = fs::read_to_string(PID_FILE)?;
            let pid_num: i32 = match pid_str.trim().parse() {
                Ok(num) => num,
                Err(_) => {
                    eprintln!("Invalid PID file.");
                    exit(1);
                }
            };
            let pid = Pid::from_raw(pid_num);

            // Send SIGTERM to the recorder process
            if let Err(e) = kill(pid, Signal::SIGTERM) {
                eprintln!("Failed to terminate recorder process: {}", e);
                exit(1);
            }

            // Wait for the process to terminate
            let mut retries = 10;
            while Path::new(PID_FILE).exists() && retries > 0 {
                thread::sleep(Duration::from_millis(500));
                retries -= 1;
            }

            if Path::new(PID_FILE).exists() {
                eprintln!("Failed to terminate recorder process.");
                exit(1);
            }

            println!("Recording stopped. Generating GIF...");

            // Generate GIF from recording data
            match generate_gif() {
                Ok(_) => {
                    println!("GIF generated as {}", GIF_FILE);
                }
                Err(e) => {
                    eprintln!("Failed to generate GIF: {}", e);
                    exit(1);
                }
            }

            // Cleanup recording file
            let _ = fs::remove_file(RECORDING_FILE);
        }
    }

    Ok(())
}

/// Function to run the recorder in the background child process
fn run_recorder() -> io::Result<()> {
    // Write PID to PID_FILE
    let pid = nix::unistd::getpid();
    fs::write(PID_FILE, pid.to_string())?;

    // Set up signal handler for graceful termination
    let running = Arc::new(Mutex::new(true));
    {
        let running = Arc::clone(&running);
        ctrlc::set_handler(move || {
            let mut running = running.lock().unwrap();
            *running = false;
        })
        .expect("Error setting Ctrl-C handler");
    }

    // Initialize recording data
    let events: Arc<Mutex<Vec<TerminalEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);
    let start_time = std::time::Instant::now();

    // Initialize PTY and spawn shell
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let mut shell = pair
        .slave
        .spawn_command(CommandBuilder::new("/bin/bash"))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    // Reader thread: reads from PTY and records output
    let reader_events = Arc::clone(&events_clone);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    thread::spawn(move || {
        let mut buffer = [0u8; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let output = String::from_utf8_lossy(&buffer[..n]).to_string();
                    let mut events = reader_events.lock().unwrap();
                    events.push(TerminalEvent {
                        timestamp: start_time.elapsed().as_millis(),
                        input: None,
                        output: Some(output),
                    });
                }
                Err(_) => break,
            }
        }
    });

    // Writer thread: reads user input and sends to PTY
    let writer_events = Arc::clone(&events_clone);
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        let mut buffer = [0u8; 1024];
        loop {
            match handle.read(&mut buffer) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    // Write to PTY using the `write` method
                    if let Err(e) = writer.write_all(&buffer[..n]) {
                        eprintln!("Failed to write to PTY: {}", e);
                        break;
                    }
                    let input_str = String::from_utf8_lossy(&buffer[..n]).to_string();
                    let mut events = writer_events.lock().unwrap();
                    events.push(TerminalEvent {
                        timestamp: start_time.elapsed().as_millis(),
                        input: Some(input_str),
                        output: None,
                    });
                }
                Err(_) => break,
            }
        }
    });

    // Periodically save recording data
    let save_events = Arc::clone(&events_clone);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(5));
        let data = save_events.lock().unwrap();
        if data.is_empty() {
            continue;
        }
        if let Ok(json) = serde_json::to_string(&*data) {
            let _ = fs::write(RECORDING_FILE, json);
        }
    });

    // Main loop: keep running until termination signal
    while *running.lock().unwrap() {
        thread::sleep(Duration::from_millis(100));
    }

    // Terminate the shell process
    shell.kill()?;

    // Wait for the shell to exit
    let _ = shell.wait();

    // Serialize recording data to RECORDING_FILE
    let recorded_events = events.lock().unwrap();
    let json = serde_json::to_string(&*recorded_events)?;
    fs::write(RECORDING_FILE, json)?;

    // Remove PID_FILE
    fs::remove_file(PID_FILE)?;

    Ok(())
}

/// Function to generate GIF from recorded terminal events
fn generate_gif() -> io::Result<()> {
    // Read recording data
    let data = fs::read_to_string(RECORDING_FILE)?;
    let events: Vec<TerminalEvent> = serde_json::from_str(&data)?;

    if events.is_empty() {
        eprintln!("No events recorded.");
        exit(1);
    }

    // Load a font
    // Ensure the font file exists at the specified path or change the path accordingly
    let font_path = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf";
    let font_data = fs::read(font_path).expect("Failed to read font file.");
    let font = Font::try_from_vec(font_data).expect("Failed to load font.");

    // Define image parameters
    let scale = Scale { x: 20.0, y: 20.0 };
    let image_width = 800;
    let image_height = 600;

    // Create a vector to hold frames
    let mut frames = Vec::new();

    // Initialize screen buffer
    let mut screen = String::new();

    // Iterate over events and render to images
    for event in events {
        if let Some(input) = event.input {
            screen.push_str(&input);
        }
        if let Some(output) = event.output {
            screen.push_str(&output);
        }

        // Render current screen to image
        let img = render_text_to_image(&screen, &font, scale, image_width, image_height)?;
        frames.push(img);
    }

    // Create GIF
    let mut gif_file = File::create(GIF_FILE)?;
    let mut encoder = GifEncoder::new(&mut gif_file, image_width as u16, image_height as u16, &[])
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    for frame in frames {
        let mut buffer: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buffer);
        let gif_frame =
            GifFrame::from_rgb_speed(image_width as u16, image_height as u16, &frame, 10);
        encoder
            .write_frame(&gif_frame)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    }

    Ok(())
}

/// Function to render text to an RGB image
fn render_text_to_image(
    text: &str,
    font: &Font,
    scale: Scale,
    width: u32,
    height: u32,
) -> io::Result<RgbImage> {
    // Create a blank black image
    let mut image = RgbImage::from_pixel(width, height, Rgb([0, 0, 0]));

    // Position to start drawing text
    let mut x = 10.0;
    let mut y = scale.y;

    for line in text.lines() {
        for glyph in font.layout(line, scale, rusttype::point(x, y)) {
            if let Some(bounding_box) = glyph.pixel_bounding_box() {
                glyph.draw(|gx, gy, gv| {
                    let px = gx + bounding_box.min.x as u32;
                    let py = gy + bounding_box.min.y as u32;
                    if px < width && py < height {
                        let pixel = image.get_pixel_mut(px, py);
                        let intensity = (gv * 255.0) as u8;
                        // Simple white text
                        *pixel = Rgb([intensity, intensity, intensity]);
                    }
                });
            }
        }
        y += scale.y + 5.0; // Move to next line
    }

    Ok(image)
}
