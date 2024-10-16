use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use clap::{arg, command, Args, Parser, Subcommand};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password};
use espflash::cli::{config::Config, connect, monitor::monitor, ConnectArgs, EspflashProgress};
use micro_rdk_installer::error::Error;
use micro_rdk_installer::nvs::data::{ViamFlashStorageData, WifiCredentials};
use micro_rdk_installer::nvs::metadata::read_nvs_metadata;
use micro_rdk_installer::nvs::partition::{NVSPartition, NVSPartitionData};
use micro_rdk_installer::nvs::request::{
    download_micro_rdk_release, populate_nvs_storage_from_app,
};
use secrecy::Secret;
use serde::Deserialize;
use tokio::runtime::Runtime;

#[derive(Deserialize, Debug)]
struct AppCloudConfig {
    r#id: String,
    app_address: String,
    secret: Secret<String>,
}

#[derive(Deserialize, Debug)]
struct AppConfig {
    cloud: AppCloudConfig,
}

#[derive(Subcommand)]
enum Commands {
    WriteFlash(WriteFlash),
    WriteCredentials(WriteCredentials),
    CreateNvsPartition(CreateNVSPartition),
    Monitor(Monitor),
}

/// Write Wi-Fi and robot credentials to the NVS storage portion of a pre-compiled
/// binary running a micro-RDK server
#[derive(Args)]
struct WriteCredentials {
    /// File path to the JSON config of the robot, downloaded from app.viam.com
    #[arg(long = "app-config")]
    config: String,
    /// File path to the compiled micro-RDK binary. The portion reserved for the NVS
    /// data partition will be edited with Wi-Fi and robot credentials
    #[arg(long = "binary-path")]
    binary_path: String,
}

/// Flash a pre-compiled binary with the micro-RDK server directly to an ESP32
/// connected to your computer via data cable
#[derive(Args)]
struct WriteFlash {
    /// File path to the JSON config of the robot, downloaded from app.viam.com
    #[arg(long = "app-config")]
    config: String,
    /// File path to the compiled micro-RDK binary. The portion reserved for the NVS
    /// data partition will be edited with wifi and robot credentials
    #[arg(long = "bin")]
    binary_path: Option<String>,
    /// Version of the compiled micro-RDK server to download.
    /// See https://github.com/viamrobotics/micro-rdk/releases for the version options
    #[arg(long = "version")]
    version: Option<String>,
    #[arg(long = "baud-rate")]
    baud_rate: Option<u32>,
    /// This opens the serial monitor immediately after flashing.
    /// The micro-RDK server logs can be viewed this way
    #[arg(long = "monitor")]
    monitor: bool,
}

/// Generate a binary of a complete NVS data partition that conatins Wi-Fi and security
/// credentials for a robot
#[derive(Args)]
struct CreateNVSPartition {
    // File path to the JSON config of the robot, downloaded from app.viam.com
    #[arg(long = "app-config")]
    config: String,
    #[arg(long = "output")]
    file_name: String,
    // Size of the NVS partition in bytes. The default here represents the size
    // declared in examples/esp32/partitions.csv (0x8000, or 32768)
    #[arg(long = "size", default_value = "32768")]
    size: usize,
}

/// Monitor a currently connected ESP32
#[derive(Args)]
struct Monitor {
    #[arg(long = "baud-rate")]
    baud_rate: Option<u32>,
}

#[derive(Parser)]
#[command(
    about = "A CLI that can flash a compilation of micro-RDK directly to an ESP32 provided configuration information"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

fn request_wifi() -> Result<WifiCredentials, Error> {
    let ssid: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Please enter WiFi SSID")
        .interact_text()
        .map_err(Error::WifiCredentialsError)?;
    let password: Secret<String> = Secret::new(
        Password::with_theme(&ColorfulTheme::default())
            .with_prompt("Please enter WiFi Password")
            .validate_with(|input: &String| -> Result<(), Error> {
                if input.len() > 64 {
                    return Err(Error::WifiPasswordTooLongError(
                        "password length limited to 64 characters or less".to_string(),
                    ));
                }
                Ok(())
            })
            .interact()
            .map_err(Error::WifiCredentialsError)?,
    );

    Ok(WifiCredentials { ssid, password })
}

fn create_nvs_partition_binary(config_path: String, size: usize) -> Result<Vec<u8>, Error> {
    let mut storage_data = ViamFlashStorageData::default();
    let config_str = fs::read_to_string(config_path).map_err(Error::FileError)?;
    let app_config: AppConfig = serde_json::from_str(&config_str)?;
    storage_data.robot_credentials.robot_id = Some(app_config.cloud.r#id.to_string());
    storage_data.robot_credentials.app_address = Some(app_config.cloud.app_address.to_string());
    storage_data.robot_credentials.robot_secret = Some(app_config.cloud.secret);
    let wifi_cred = request_wifi()?;
    storage_data.wifi = Some(wifi_cred);
    populate_nvs_storage_from_app(&mut storage_data)?;
    let part = &mut NVSPartition::from_storage_data(storage_data, size)?;
    Ok(NVSPartitionData::try_from(part)?.to_bytes())
}

fn write_credentials_to_app_binary(
    binary_path: PathBuf,
    nvs_data: &[u8],
    nvs_size: u64,
    nvs_start_address: u64,
) -> Result<(), Error> {
    let mut app_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(binary_path)
        .map_err(Error::FileError)?;
    let file_len = app_file.metadata().map_err(Error::FileError)?.len();
    if (nvs_start_address + nvs_size) >= file_len {
        return Err(Error::BinaryEditError(file_len));
    }
    app_file
        .seek(SeekFrom::Start(nvs_start_address))
        .map_err(Error::FileError)?;
    app_file.write_all(nvs_data).map_err(Error::FileError)?;
    Ok(())
}

fn flash(binary_path: PathBuf, should_monitor: bool, baud_rate: Option<u32>) -> Result<(), Error> {
    let connect_args = ConnectArgs {
        baud: Some(baud_rate.unwrap_or(460800)),
        // let espflash auto-detect the port
        port: None,
        no_stub: false,
    };
    let conf = Config::default();
    let mut flasher = connect(&connect_args, &conf).map_err(|_| Error::FlashConnect)?;
    let mut f = File::open(binary_path).map_err(Error::FileError)?;
    let size = f.metadata().map_err(Error::FileError)?.len();
    let mut buffer = Vec::with_capacity(
        size.try_into()
            .map_err(|_| Error::BinaryBufferError(size))?,
    );
    f.read_to_end(&mut buffer).map_err(Error::FileError)?;
    flasher
        .write_bin_to_flash(0x00, &buffer, Some(&mut EspflashProgress::default()))
        .map_err(Error::EspFlashError)?;
    if should_monitor {
        let pid = flasher.get_usb_pid().map_err(Error::EspFlashError)?;
        monitor(flasher.into_interface(), None, pid, 115_200)
            .map_err(|err| Error::MonitorError(err.to_string()))?;
    }
    Ok(())
}

fn monitor_esp32(baud_rate: Option<u32>) -> Result<(), Error> {
    let connect_args = ConnectArgs {
        baud: Some(baud_rate.unwrap_or(460800)),
        // let espflash auto-detect the port
        port: None,
        no_stub: false,
    };
    let conf = Config::default();
    let flasher = connect(&connect_args, &conf).map_err(|_| Error::FlashConnect)?;
    let pid = flasher.get_usb_pid().map_err(Error::EspFlashError)?;
    monitor(flasher.into_interface(), None, pid, 115_200)
        .map_err(|err| Error::MonitorError(err.to_string()))?;
    Ok(())
}

fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    match &cli.command {
        Some(Commands::WriteCredentials(args)) => {
            let app_path = PathBuf::from(args.binary_path.clone());
            let nvs_metadata = read_nvs_metadata(app_path.clone())?;
            let nvs_data =
                create_nvs_partition_binary(args.config.to_string(), nvs_metadata.size as usize)?;
            write_credentials_to_app_binary(
                app_path,
                &nvs_data,
                nvs_metadata.size,
                nvs_metadata.start_address,
            )?;
        }
        Some(Commands::WriteFlash(args)) => {
            let tmp_dir = tempfile::Builder::new()
                .prefix("micro-rdk-bin")
                .tempdir()
                .map_err(Error::FileError)?;
            let app_path = match args.binary_path.clone() {
                Some(path) => PathBuf::from(path),
                None => {
                    let rt = Runtime::new().map_err(Error::AsyncError)?;
                    rt.block_on(download_micro_rdk_release(&tmp_dir, args.version.clone()))?
                }
            };
            let nvs_metadata = read_nvs_metadata(app_path.clone())?;
            let nvs_data =
                create_nvs_partition_binary(args.config.to_string(), nvs_metadata.size as usize)?;
            write_credentials_to_app_binary(
                app_path.clone(),
                &nvs_data,
                nvs_metadata.size,
                nvs_metadata.start_address,
            )?;
            flash(app_path, args.monitor, args.baud_rate)?;
        }
        Some(Commands::CreateNvsPartition(args)) => {
            let mut file = File::create(&args.file_name).map_err(Error::FileError)?;
            file.write_all(&create_nvs_partition_binary(
                args.config.to_string(),
                args.size,
            )?)
            .map_err(Error::FileError)?;
        }
        Some(Commands::Monitor(args)) => monitor_esp32(args.baud_rate)?,
        None => return Err(Error::NoCommandError),
    };
    Ok(())
}
