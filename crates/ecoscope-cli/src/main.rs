use std::{collections::BTreeMap, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use ecoscope_core::{
    DatasetId, ProfileTrajectoryRecipeV1, ProfileValueSpec, VerticalAxisSpec, VerticalDirection,
};
use ecoscope_provider_api::EcologicalDataProvider;
use ecoscope_provider_process::{ProcessProvider, ProcessProviderConfig, discover_configs};
use ecoscope_service::EcoScopeService;
use tracing_subscriber::EnvFilter;

mod demo;

#[derive(Debug, Parser)]
#[command(
    name = "ecoscope",
    version,
    about = "Agent-native ecological data workbench and MCP server",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize local state and show MCP registration instructions.
    Setup,
    /// Run the registered MCP server over stdin/stdout.
    Mcp,
    /// Store a NEON API token directly in the operating-system keychain.
    ConnectNeon,
    /// Remove the stored NEON API token.
    DisconnectNeon,
    /// Register EcoScope as a user-level MCP server in a supported host.
    Register {
        #[arg(value_enum)]
        host: McpHost,
    },
    /// Install or inspect language-neutral ecological provider subprocesses.
    Provider {
        #[command(subcommand)]
        command: ProviderCommands,
    },
    /// Import a local scientific file selected by the user.
    Import {
        #[arg(value_name = "FILE")]
        path: PathBuf,
    },
    /// List imported and materialized datasets.
    Datasets,
    /// Print an immutable dataset manifest.
    Inspect { dataset_id: String },
    /// Preview a local tabular dataset.
    Preview {
        dataset_id: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Create a persistent semantic view.
    CreateView {
        #[arg(long)]
        name: String,
        #[arg(required = true)]
        dataset_ids: Vec<String>,
    },
    /// Configure explicit HDF5 cube and band semantics for a view layer.
    ConfigureHyperspectral {
        view_id: String,
        #[arg(long, default_value = "layer_1")]
        layer_id: String,
        #[arg(long)]
        hdf5_dataset: String,
        #[arg(long)]
        wavelength_dataset: Option<String>,
        #[arg(long)]
        band: Option<u32>,
        #[arg(long)]
        red_band: Option<u32>,
        #[arg(long)]
        green_band: Option<u32>,
        #[arg(long)]
        blue_band: Option<u32>,
    },
    /// Configure explicit rank-3 HDF5, NetCDF, or Zarr cube semantics.
    ConfigureCube {
        view_id: String,
        #[arg(long, default_value = "layer_1")]
        layer_id: String,
        #[arg(long)]
        cube_array: String,
        #[arg(long, default_value_t = 0)]
        y_axis: u32,
        #[arg(long, default_value_t = 1)]
        x_axis: u32,
        #[arg(long, default_value_t = 2)]
        spectral_axis: u32,
        #[arg(long)]
        wavelength_dataset: Option<String>,
        #[arg(long)]
        band: Option<u32>,
        #[arg(long)]
        red_band: Option<u32>,
        #[arg(long)]
        green_band: Option<u32>,
        #[arg(long)]
        blue_band: Option<u32>,
    },
    /// Configure a validated linked trajectory map and vertical profile.
    ConfigureProfileTrajectory {
        view_id: String,
        #[arg(long, default_value = "layer_1")]
        layer_id: String,
        #[arg(long)]
        trajectory_id_field: String,
        #[arg(long)]
        profile_id_field: String,
        #[arg(long)]
        time_field: Option<String>,
        #[arg(long)]
        latitude_field: String,
        #[arg(long)]
        longitude_field: String,
        #[arg(long)]
        vertical_field: String,
        #[arg(long, value_enum)]
        vertical_direction: VerticalDirectionArg,
        #[arg(long)]
        vertical_unit: Option<String>,
        #[arg(long)]
        value_field: String,
        #[arg(long)]
        value_unit: Option<String>,
        #[arg(long)]
        qc_field: Option<String>,
        #[arg(long = "accept-qc")]
        accepted_qc: Vec<String>,
    },
    /// Generate a Rerun recording from a semantic view.
    Render {
        view_id: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Re-run the exact scientific query represented by a saved visual selection.
    QuerySelection {
        selection_id: String,
        #[arg(long)]
        dataset_id: Option<String>,
        #[arg(long, default_value_t = 100_000)]
        point_limit: u64,
    },
    /// Render and open a semantic view in Rerun.
    Open {
        view_id: String,
        /// Use the native Rerun viewer instead of the browser explorer.
        #[arg(long)]
        native: bool,
    },
    /// Run the loopback-only browser explorer until interrupted.
    Serve {
        view_id: String,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long)]
        open: bool,
    },
    /// Diagnose database, credentials, MCP, and viewer readiness.
    Doctor,
    /// Create and open a reproducible multimodal demonstration.
    Demo {
        #[arg(value_enum, default_value_t = DemoKind::Synthetic)]
        kind: DemoKind,
        /// Permit the roughly 224 MB official NEON teaching-data download.
        #[arg(long)]
        accept_download: bool,
        /// Render the view without opening and serving the browser explorer.
        #[arg(long)]
        no_open: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DemoKind {
    Synthetic,
    Neon,
    ProfileTrajectory,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpHost {
    Codex,
    Claude,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum VerticalDirectionArg {
    PositiveDown,
    PositiveUp,
}

impl From<VerticalDirectionArg> for VerticalDirection {
    fn from(value: VerticalDirectionArg) -> Self {
        match value {
            VerticalDirectionArg::PositiveDown => Self::PositiveDown,
            VerticalDirectionArg::PositiveUp => Self::PositiveUp,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ProviderCommands {
    /// Validate and install a provider JSON configuration.
    Install {
        #[arg(value_name = "CONFIG")]
        config: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// List built-in manifests and negotiate installed provider manifests.
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(matches!(cli.command, Commands::Mcp));
    let service = EcoScopeService::discover().context("opening EcoScope state")?;

    match cli.command {
        Commands::Setup => setup(&service)?,
        Commands::Mcp => ecoscope_mcp::run_stdio(service).await?,
        Commands::ConnectNeon => connect_neon()?,
        Commands::DisconnectNeon => {
            ecoscope_mcp::remove_neon_token()?;
            println!("Removed the NEON credential from the operating-system keychain.");
        }
        Commands::Register { host } => register_mcp(host)?,
        Commands::Provider { command } => manage_provider(&service, command).await?,
        Commands::Import { path } => {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("cannot open {}", path.display()))?;
            let manifest = service.import_local_file(&canonical).await?;
            print_json(&manifest)?;
        }
        Commands::Datasets => print_json(&service.list_manifests()?)?,
        Commands::Inspect { dataset_id } => {
            print_json(&service.get_manifest(&dataset_id)?)?;
        }
        Commands::Preview { dataset_id, limit } => {
            print_json(&service.preview_dataset(&dataset_id, limit.min(2_000))?)?;
        }
        Commands::CreateView { name, dataset_ids } => {
            let view =
                service.create_view(name, dataset_ids.into_iter().map(DatasetId).collect())?;
            print_json(&view)?;
        }
        Commands::ConfigureHyperspectral {
            view_id,
            layer_id,
            hdf5_dataset,
            wavelength_dataset,
            band,
            red_band,
            green_band,
            blue_band,
        } => {
            let view = service.get_view(&view_id)?;
            let mut encoding = BTreeMap::from([
                ("hdf5_dataset".into(), serde_json::json!(hdf5_dataset)),
                ("spectral_axis".into(), serde_json::json!(2)),
            ]);
            for (key, value) in [
                (
                    "wavelength_dataset",
                    wavelength_dataset.map(serde_json::Value::from),
                ),
                ("band", band.map(serde_json::Value::from)),
                ("red_band", red_band.map(serde_json::Value::from)),
                ("green_band", green_band.map(serde_json::Value::from)),
                ("blue_band", blue_band.map(serde_json::Value::from)),
            ] {
                if let Some(value) = value {
                    encoding.insert(key.into(), value);
                }
            }
            let configured =
                service.configure_layer_encoding(&view_id, view.revision, &layer_id, encoding)?;
            print_json(&configured)?;
        }
        Commands::ConfigureCube {
            view_id,
            layer_id,
            cube_array,
            y_axis,
            x_axis,
            spectral_axis,
            wavelength_dataset,
            band,
            red_band,
            green_band,
            blue_band,
        } => {
            if y_axis == x_axis || y_axis == spectral_axis || x_axis == spectral_axis {
                bail!("y-axis, x-axis, and spectral-axis must be distinct");
            }
            let has_single = band.is_some();
            let rgb_count = [red_band, green_band, blue_band]
                .into_iter()
                .flatten()
                .count();
            if has_single == (rgb_count == 3) || (!has_single && rgb_count != 3) {
                bail!("provide either --band or all of --red-band, --green-band, and --blue-band");
            }
            let view = service.get_view(&view_id)?;
            let mut encoding = BTreeMap::from([
                ("cube_array".into(), serde_json::json!(cube_array)),
                ("y_axis".into(), serde_json::json!(y_axis)),
                ("x_axis".into(), serde_json::json!(x_axis)),
                ("spectral_axis".into(), serde_json::json!(spectral_axis)),
            ]);
            if let Some(wavelength_dataset) = wavelength_dataset {
                encoding.insert(
                    "wavelength_dataset".into(),
                    serde_json::json!(wavelength_dataset),
                );
            }
            for (key, value) in [
                ("band", band),
                ("red_band", red_band),
                ("green_band", green_band),
                ("blue_band", blue_band),
            ] {
                if let Some(value) = value {
                    encoding.insert(key.into(), serde_json::json!(value));
                }
            }
            let configured =
                service.configure_layer_encoding(&view_id, view.revision, &layer_id, encoding)?;
            print_json(&configured)?;
        }
        Commands::ConfigureProfileTrajectory {
            view_id,
            layer_id,
            trajectory_id_field,
            profile_id_field,
            time_field,
            latitude_field,
            longitude_field,
            vertical_field,
            vertical_direction,
            vertical_unit,
            value_field,
            value_unit,
            qc_field,
            accepted_qc,
        } => {
            let view = service.get_view(&view_id)?;
            let configured = service.configure_profile_trajectory_view(
                &view_id,
                view.revision,
                &layer_id,
                ProfileTrajectoryRecipeV1 {
                    trajectory_id_field,
                    profile_id_field,
                    time_field,
                    latitude_field,
                    longitude_field,
                    vertical: VerticalAxisSpec {
                        field: vertical_field,
                        direction: vertical_direction.into(),
                        unit: vertical_unit,
                    },
                    value: ProfileValueSpec {
                        field: value_field,
                        unit: value_unit,
                        qc_field,
                        accepted_qc,
                    },
                },
            )?;
            print_json(&configured)?;
        }
        Commands::Render { view_id, output } => {
            let output =
                output.unwrap_or_else(|| service.paths().views_dir.join(format!("{view_id}.rrd")));
            let path = ecoscope_rerun::write_recording(&service, &view_id, output)?;
            println!("{}", path.display());
        }
        Commands::QuerySelection {
            selection_id,
            dataset_id,
            point_limit,
        } => {
            let result = service
                .query_selection(&selection_id, dataset_id.as_deref(), point_limit)
                .await?;
            print_json(&result)?;
        }
        Commands::Open { view_id, native } => {
            if native {
                let output = service.paths().views_dir.join(format!("{view_id}.rrd"));
                let path = ecoscope_rerun::write_recording(&service, &view_id, output)?;
                ecoscope_rerun::open_recording(&path)?;
                println!("Opened {view_id} in native Rerun.");
            } else {
                ecoscope_web::serve(
                    service,
                    ecoscope_web::ServeOptions {
                        view_id,
                        port: 0,
                        open_browser: true,
                    },
                )
                .await?;
            }
        }
        Commands::Serve {
            view_id,
            port,
            open,
        } => {
            ecoscope_web::serve(
                service,
                ecoscope_web::ServeOptions {
                    view_id,
                    port,
                    open_browser: open,
                },
            )
            .await?;
        }
        Commands::Doctor => doctor(&service)?,
        Commands::Demo {
            kind,
            accept_download,
            no_open,
        } => {
            let demo = match kind {
                DemoKind::Synthetic => demo::synthetic(&service).await?,
                DemoKind::Neon => demo::official_neon(&service, accept_download).await?,
                DemoKind::ProfileTrajectory => demo::profile_trajectory(&service).await?,
            };
            print_json(&demo)?;
            if !no_open {
                ecoscope_web::serve(
                    service,
                    ecoscope_web::ServeOptions {
                        view_id: demo.view_id,
                        port: 0,
                        open_browser: true,
                    },
                )
                .await?;
            }
        }
    }
    Ok(())
}

fn init_logging(mcp_mode: bool) {
    let default_filter = if mcp_mode { "warn" } else { "info" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .init();
}

fn setup(service: &EcoScopeService) -> Result<()> {
    let executable = std::env::current_exe()?;
    println!(
        "EcoScope state initialized at {}",
        service.paths().data_dir.display()
    );
    println!();
    println!("Register with Codex:");
    println!(
        "  codex mcp add ecoscope -- {} mcp",
        shell_display(&executable)
    );
    println!();
    println!("Register with Claude Code:");
    println!(
        "  claude mcp add --scope user ecoscope -- {} mcp",
        shell_display(&executable)
    );
    println!();
    println!("Or run `ecoscope register codex` / `ecoscope register claude`.");
    println!("Run `ecoscope connect-neon` when you are ready to download NEON data.");
    println!();
    println!("Readiness report:");
    doctor(service)?;
    Ok(())
}

async fn manage_provider(service: &EcoScopeService, command: ProviderCommands) -> Result<()> {
    match command {
        ProviderCommands::Install { config, force } => {
            let source = config
                .canonicalize()
                .with_context(|| format!("cannot open {}", config.display()))?;
            let parsed = ProcessProviderConfig::from_file(&source)?;
            if ecoscope_mcp::built_in_provider_manifests()?
                .iter()
                .any(|provider| provider.provider_id == parsed.expected_provider_id)
            {
                bail!(
                    "provider ID {} is reserved by a built-in provider",
                    parsed.expected_provider_id
                );
            }
            let provider = ProcessProvider::spawn(parsed.clone())
                .await
                .context("provider handshake failed")?;
            let destination = service
                .paths()
                .providers_dir
                .join(format!("{}.json", parsed.expected_provider_id));
            if destination.exists() && !force {
                bail!(
                    "provider {} is already installed; pass --force to replace its configuration",
                    parsed.expected_provider_id
                );
            }
            std::fs::copy(&source, &destination).with_context(|| {
                format!(
                    "copying provider configuration to {}",
                    destination.display()
                )
            })?;
            print_json(&serde_json::json!({
                "installed": provider.manifest(),
                "configuration": destination
            }))?;
        }
        ProviderCommands::List => {
            let mut providers = ecoscope_mcp::built_in_provider_manifests()?;
            let mut unavailable = Vec::new();
            for config in discover_configs(&service.paths().providers_dir)? {
                let provider_id = config.expected_provider_id.clone();
                if providers
                    .iter()
                    .any(|provider| provider.provider_id == provider_id)
                {
                    unavailable.push(serde_json::json!({
                        "provider_id": provider_id,
                        "error": "provider ID is reserved by a built-in provider"
                    }));
                    continue;
                }
                match ProcessProvider::spawn(config).await {
                    Ok(provider) => providers.push(provider.manifest()),
                    Err(error) => unavailable.push(serde_json::json!({
                        "provider_id": provider_id,
                        "error": error.to_string()
                    })),
                }
            }
            print_json(&serde_json::json!({
                "providers": providers,
                "unavailable": unavailable
            }))?;
        }
    }
    Ok(())
}

fn connect_neon() -> Result<()> {
    println!("The token is entered directly here and will not be echoed or sent to a model.");
    let token = rpassword::prompt_password("NEON API token: ")?;
    ecoscope_mcp::store_neon_token(&token)?;
    println!("Stored the NEON credential in the operating-system keychain.");
    Ok(())
}

fn register_mcp(host: McpHost) -> Result<()> {
    let executable = std::env::current_exe()?;
    if registration_status(host)?.is_some() {
        println!("EcoScope is already registered with {host:?}; existing configuration retained.");
        return Ok(());
    }
    let mut command = match host {
        McpHost::Codex => {
            let mut command = Command::new("codex");
            command.args(["mcp", "add", "ecoscope", "--"]);
            command
        }
        McpHost::Claude => {
            let mut command = Command::new("claude");
            command.args(["mcp", "add", "--scope", "user", "ecoscope", "--"]);
            command
        }
    };
    let status = command
        .arg(executable)
        .arg("mcp")
        .status()
        .with_context(|| {
            format!(
                "could not run the {} CLI",
                match host {
                    McpHost::Codex => "Codex",
                    McpHost::Claude => "Claude",
                }
            )
        })?;
    if !status.success() {
        bail!("MCP host registration exited with {status}");
    }
    if registration_status(host)?.is_none() {
        bail!("the {host:?} CLI returned success but EcoScope was not present afterward");
    }
    println!("Registered EcoScope with {host:?}.");
    Ok(())
}

fn registration_status(host: McpHost) -> Result<Option<String>> {
    let output = match host {
        McpHost::Codex => Command::new("codex")
            .args(["mcp", "get", "ecoscope", "--json"])
            .output(),
        McpHost::Claude => Command::new("claude")
            .args(["mcp", "get", "ecoscope"])
            .output(),
    }
    .with_context(|| {
        format!(
            "could not run the {} CLI",
            match host {
                McpHost::Codex => "Codex",
                McpHost::Claude => "Claude",
            }
        )
    })?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    let message = String::from_utf8_lossy(&output.stderr);
    if message.contains("No MCP server named") || message.contains("not found") {
        Ok(None)
    } else {
        bail!("could not inspect existing {host:?} registration: {message}")
    }
}

fn doctor(service: &EcoScopeService) -> Result<()> {
    let health = service.health()?;
    let rerun = Command::new("rerun").arg("--version").output();
    let rerun_status = rerun
        .as_ref()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    let report = serde_json::json!({
        "service": health,
        "neon_connected": ecoscope_mcp::neon_connected(),
        "rerun": {
            "installed": rerun_status.is_some(),
            "version": rerun_status,
            "required_recording_version": ecoscope_rerun::PINNED_RERUN_VERSION,
        },
        "mcp": {
            "transport": "stdio",
            "spec": "2026-07-28"
        }
    });
    print_json(&report)
}

fn shell_display(path: &std::path::Path) -> String {
    let value = path.display().to_string();
    if value.contains(' ') {
        format!("'{value}'")
    } else {
        value
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_trajectory_command_parses_the_public_cli_contract() {
        let cli = Cli::try_parse_from([
            "ecoscope",
            "configure-profile-trajectory",
            "view_1",
            "--layer-id",
            "layer_1",
            "--trajectory-id-field",
            "platform_number",
            "--profile-id-field",
            "cycle_number",
            "--time-field",
            "time",
            "--latitude-field",
            "latitude",
            "--longitude-field",
            "longitude",
            "--vertical-field",
            "pres",
            "--vertical-direction",
            "positive-down",
            "--vertical-unit",
            "decibar",
            "--value-field",
            "temp_adjusted",
            "--value-unit",
            "degree_Celsius",
            "--qc-field",
            "temp_adjusted_qc",
            "--accept-qc",
            "1",
            "--accept-qc",
            "2",
        ])
        .unwrap();
        let Commands::ConfigureProfileTrajectory {
            accepted_qc,
            vertical_direction,
            ..
        } = cli.command
        else {
            panic!("wrong command parsed");
        };
        assert_eq!(accepted_qc, ["1", "2"]);
        assert!(matches!(
            vertical_direction,
            VerticalDirectionArg::PositiveDown
        ));
    }
}
