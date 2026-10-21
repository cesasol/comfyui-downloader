use crate::config::Config;
use crate::ipc::protocol::{
    EnrichedModel, FileVariantInfo, QueueItem, TemplateListing, VersionInfo,
};
use crate::ipc::{IpcClient, Request, Response};
use crate::secrets;
use crate::templates::{TemplateBundle, TemplateFilter};
use crate::vram::{Feasibility, WorkloadKind};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use console::{Alignment, Key, Term, measure_text_width, pad_str, style, truncate_str};
use image::DynamicImage;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "comfyui-dl",
    about = "CLI client for the comfyui-downloader daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Add {
        url: String,
        #[arg(long)]
        model_type: Option<String>,
        /// Browsing folder to file this one file under, overriding the label
        /// derived from source metadata.
        #[arg(long)]
        family: Option<String>,
    },
    Status,
    /// List the catalogued models as a table, with preview thumbnails on
    /// terminals that can draw inline images.
    List {
        /// Print the daemon's raw model list as JSON instead of the table.
        /// Never paginated.
        #[arg(long)]
        json: bool,
        /// When to draw preview thumbnails inline.
        #[arg(long, value_enum, default_value_t = ImageMode::Auto, value_name = "WHEN")]
        images: ImageMode,
        /// Which page to show, counting from 1.
        #[arg(long, value_name = "N", default_value_t = 1)]
        page: usize,
        /// How many models a page holds.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_PAGE_SIZE)]
        per_page: usize,
        /// Show every model at once instead of one page.
        #[arg(long, conflicts_with_all = ["page", "per_page"])]
        all: bool,
    },
    /// Delete a catalogued model by ID (full UUID or unique prefix).
    Delete {
        id: String,
    },
    CheckUpdates,
    /// Cancel a queued or active download by ID (full UUID or unique prefix).
    Cancel {
        id: String,
    },
    /// Report where the catalog and the models directory disagree, and whether
    /// the daemon's credentials are accepted.
    Doctor {
        /// Collapse duplicate rows and drop dangling ones that other rows
        /// already account for.
        #[arg(long)]
        repair: bool,
    },
    /// Inspect the model-family browsing aliases, or merge in the defaults
    /// shipped with a newer release.
    Families {
        /// Show what a merge would change and apply it.
        #[arg(long)]
        merge: bool,
        /// Apply the merge instead of only previewing it.
        #[arg(long, requires = "merge")]
        apply: bool,
        /// Also restore aliases previously removed from the snapshot.
        #[arg(long, requires = "apply")]
        restore_removed: bool,
    },
    /// Store an API credential in the system keyring (Secret Service).
    ///
    /// Omit KEY to enter it at a hidden prompt; passing it inline leaks the
    /// secret into your shell history. The key can also be piped on stdin.
    /// The key is checked against the service before it is stored.
    SetKey {
        /// The credential value. Omit to type it at a hidden prompt.
        key: Option<String>,
        /// Which credential to store: civitai (default) or huggingface.
        #[arg(long, value_name = "SERVICE", default_value = "civitai")]
        service: String,
    },
    Updates,
    DownloadVersion {
        model_id: u64,
        version_id: u64,
    },
    /// Re-queue catalogued models whose files are missing on disk.
    RedownloadMissing {
        /// Re-queue all catalogued models, even ones still present on disk.
        #[arg(long)]
        all: bool,
    },
    /// Browse the ComfyUI default workflow templates and download their models.
    ///
    /// Each template is judged against the local GPU: bundles that fit
    /// entirely in VRAM, bundles that fit only with the text encoders and VAE
    /// on the CPU, and bundles that cannot run at all (hidden by default).
    Templates {
        /// Free text matched against title, description, tags and model family.
        query: Option<String>,
        /// Generation type: image, video, audio, 3d or llm (repeatable).
        #[arg(long = "type", value_name = "KIND")]
        kind: Vec<String>,
        /// Task tag, e.g. "image edit" or "text to video" (repeatable).
        #[arg(long, value_name = "TAG")]
        task: Vec<String>,
        /// Model family, e.g. z-image-turbo (repeatable).
        #[arg(long, value_name = "FAMILY")]
        model: Vec<String>,
        /// Exact template name (repeatable).
        #[arg(long, value_name = "NAME")]
        name: Vec<String>,
        /// Only bundles that fit entirely in VRAM.
        #[arg(long)]
        comfortable_only: bool,
        /// Include cloud-API templates, which download no weights.
        #[arg(long)]
        include_api: bool,
        /// Include bundles that cannot run on this GPU.
        #[arg(long)]
        include_unrunnable: bool,
        /// Re-fetch the catalog instead of using the local cache.
        #[arg(long)]
        refresh: bool,
        /// Print the resolved listing as JSON instead of picking.
        #[arg(long)]
        json: bool,
        /// Queue every matching template without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// When `list` should draw preview thumbnails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImageMode {
    /// Draw them when stdout is a terminal that speaks an inline-image
    /// protocol.
    Auto,
    /// Never draw them.
    Never,
    /// Draw them even when no support was detected.
    Always,
}

/// Cell box every thumbnail occupies. Wide enough to recognise the preview,
/// short enough that a model still reads as one entry in a long list.
const THUMB_CELLS_WIDE: u32 = 14;
const THUMB_CELLS_TALL: u32 = 7;

/// Generous pixel size of one terminal cell, used to scale a preview down
/// before it is transmitted. The protocols send whole images and let the
/// terminal shrink them, so a full-resolution preview would put megabytes of
/// base64 on the wire for a thumbnail a few cells wide.
const CELL_PIXELS_WIDE: u32 = 10;
const CELL_PIXELS_TALL: u32 = 20;

/// Columns a card reserves on the left: the thumbnail, a space, the border
/// rule, and a space before the text.
const CARD_GUTTER: usize = THUMB_CELLS_WIDE as usize + 3;

const DEFAULT_PAGE_SIZE: usize = 10;

async fn run_list(
    client: &mut IpcClient,
    json: bool,
    images: ImageMode,
    page: usize,
    per_page: usize,
    all: bool,
) -> Result<()> {
    // The JSON form stays on `ListModels`: catalog rows carry placement
    // provenance and template labels that the enriched view drops. It is the
    // scripting interface, so it is never paginated.
    if json {
        let response = client.send(&Request::ListModels).await?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }

    let data = ok_data(client.send(&Request::ListModelsEnriched).await?)?;
    let models: Vec<EnrichedModel> =
        serde_json::from_value(data).context("parsing the enriched model list")?;

    if models.is_empty() {
        println!("No models downloaded yet. Add one with `comfyui-dl add <url>`.");
        return Ok(());
    }

    let rows: Vec<ModelRow> = models.iter().map(ModelRow::from_model).collect();
    let total: u64 = models.iter().filter_map(|m| m.file_size).sum();
    let window = Page::of(rows.len(), page, per_page, all)?;

    println!(
        "{} model{} \u{b7} {}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        format_bytes(total)
    );
    if let Some(line) = window.caption() {
        println!("{}", style(line).dim());
    }
    println!();

    // Not a terminal means a pipe, where neither image escapes nor a width
    // guess belong, so nothing is truncated either.
    let term_width = Term::stdout().size_checked().map(|(_, cols)| cols as usize);
    let shown = &rows[window.start..window.end];

    match thumbnail_protocol(images) {
        Some(protocol) => print_model_cards(shown, term_width.unwrap_or(100), protocol),
        None => {
            print_model_table(shown, term_width.unwrap_or(usize::MAX));
            Ok(())
        }
    }
}

/// The slice of the catalog one run prints.
#[derive(Debug, PartialEq, Eq)]
struct Page {
    start: usize,
    end: usize,
    total: usize,
    /// `None` when every model is on show and there is nothing to page through.
    next_page: Option<usize>,
}

impl Page {
    fn of(total: usize, page: usize, per_page: usize, all: bool) -> Result<Self> {
        if all || per_page == 0 {
            return Ok(Self {
                start: 0,
                end: total,
                total,
                next_page: None,
            });
        }
        if page == 0 {
            bail!("--page counts from 1");
        }
        let pages = total.div_ceil(per_page);
        if page > pages {
            bail!("--page {page} is past the end; {total} model(s) make {pages} page(s)");
        }
        let start = (page - 1) * per_page;
        let end = (start + per_page).min(total);
        Ok(Self {
            start,
            end,
            total,
            next_page: (page < pages).then_some(page + 1),
        })
    }

    /// The "showing x-y of n" line, or `None` when the page is the whole list.
    fn caption(&self) -> Option<String> {
        if self.start == 0 && self.end == self.total {
            return None;
        }
        let mut caption = format!("showing {}-{} of {}", self.start + 1, self.end, self.total);
        match self.next_page {
            Some(next) => caption.push_str(&format!(" \u{b7} --page {next} for the next")),
            None => caption.push_str(" \u{b7} --all for every model"),
        }
        Some(caption)
    }
}

/// How a preview gets onto the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageProtocol {
    Kitty,
    ITerm,
    /// Coloured half-blocks. Not a graphics protocol and far coarser, but it
    /// needs nothing of the terminal beyond ANSI colour.
    HalfBlocks,
}

/// Pick the protocol to draw previews with, or `None` to print the table.
///
/// Only a terminal that names itself in the environment is ever asked to
/// answer kitty's graphics handshake: that handshake reads a reply from the
/// terminal and hangs forever on a tty that stays silent, which is what a
/// pty with nothing driving it does.
fn thumbnail_protocol(mode: ImageMode) -> Option<ImageProtocol> {
    if mode == ImageMode::Never || !std::io::stdout().is_terminal() {
        return None;
    }
    // This check only reads environment variables, so it cannot block.
    if viuer::is_iterm_supported() {
        return Some(ImageProtocol::ITerm);
    }
    if kitty_announced_itself() && kitty_answered_its_handshake() {
        return Some(ImageProtocol::Kitty);
    }
    // `always` is for a terminal we could not identify. Neither protocol can
    // be emitted blind -- kitty needs its handshake and an unannounced iTerm
    // would just print the escape -- so draw the coarse version instead.
    (mode == ImageMode::Always).then_some(ImageProtocol::HalfBlocks)
}

/// Whether the environment names a terminal that implements the kitty
/// graphics protocol, and so will answer its handshake.
fn kitty_announced_itself() -> bool {
    announces_kitty(
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
        std::env::var("TERM").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
    )
}

fn announces_kitty(kitty_window_id: bool, term: Option<&str>, term_program: Option<&str>) -> bool {
    kitty_window_id
        || term.is_some_and(|term| term.contains("kitty"))
        || term_program.is_some_and(|program| program.eq_ignore_ascii_case("ghostty"))
}

/// A real terminal replies in microseconds, and even a slow link answers well
/// inside this.
const KITTY_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Run kitty's handshake under a deadline.
///
/// The handshake reads the terminal's reply with no timeout of its own, so a
/// tty that answers neither it nor the device-attributes query behind it would
/// wedge the command for good. Running it on a thread we are willing to
/// abandon bounds that: the query has already been written by the time we stop
/// waiting, so a stranded thread emits nothing more and ends with the process.
fn kitty_answered_its_handshake() -> bool {
    let (answer, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = answer.send(viuer::get_kitty_support() != viuer::KittySupport::None);
    });
    wait.recv_timeout(KITTY_HANDSHAKE_TIMEOUT).unwrap_or(false)
}

/// One catalogued model as plain, unstyled display cells.
struct ModelRow {
    id: String,
    name: String,
    version: String,
    model_type: String,
    base_model: String,
    size: String,
    added: String,
    path: String,
    preview: Option<PathBuf>,
}

impl ModelRow {
    fn from_model(model: &EnrichedModel) -> Self {
        let id = model.id.to_string();
        let path = model.dest_path.clone().unwrap_or_default();
        // Sidecar metadata is the best name; a bare filename beats the URL.
        let name = model
            .model_name
            .clone()
            .or_else(|| {
                Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| model.url.clone());
        Self {
            id: short_id(&id).to_string(),
            name,
            version: model.version_name.clone().unwrap_or_default(),
            model_type: model
                .model_type
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            base_model: model.base_model.clone().unwrap_or_default(),
            size: model.file_size.map(format_bytes).unwrap_or_default(),
            added: model.created_at.format("%Y-%m-%d").to_string(),
            path,
            preview: model.preview_path.as_deref().map(PathBuf::from),
        }
    }
}

/// Widths of the listing table's variable columns, in display columns.
struct Columns {
    name: usize,
    kind: usize,
    base: usize,
    size: usize,
}

/// Size every column to its widest cell, then give the name column whatever
/// the terminal has left over.
fn columns_for(rows: &[ModelRow], term_width: usize) -> Columns {
    let widest = |cell: fn(&ModelRow) -> &str, heading: usize| {
        rows.iter()
            .map(|r| measure_text_width(cell(r)))
            .chain([heading])
            .max()
            .unwrap_or(heading)
    };
    let kind = widest(|r| &r.model_type, 4);
    let base = widest(|r| &r.base_model, 4);
    let size = widest(|r| &r.size, 4);
    let added = 10;
    // Fixed chrome: the 8-column id, the date, and the five 2-space gaps.
    let fixed = 8 + added + kind + base + size + 10;
    let name = widest(|r| &r.name, 5).min(term_width.saturating_sub(fixed).max(12));
    Columns {
        name,
        kind,
        base,
        size,
    }
}

fn print_model_table(rows: &[ModelRow], term_width: usize) {
    let cols = columns_for(rows, term_width);
    let header = format!(
        "{}  {}  {}  {}  {}  {}",
        pad_str("ID", 8, Alignment::Left, None),
        pad_str("MODEL", cols.name, Alignment::Left, None),
        pad_str("TYPE", cols.kind, Alignment::Left, None),
        pad_str("BASE", cols.base, Alignment::Left, None),
        pad_str("SIZE", cols.size, Alignment::Right, None),
        "ADDED",
    );
    println!("{}", style(header).bold().underlined());

    for row in rows {
        println!(
            "{}  {}  {}  {}  {}  {}",
            style(pad_str(&row.id, 8, Alignment::Left, None)).dim(),
            fit(&row.name, cols.name, Alignment::Left),
            style(fit(&row.model_type, cols.kind, Alignment::Left)).cyan(),
            style(fit(&row.base_model, cols.base, Alignment::Left)).dim(),
            style(pad_str(&row.size, cols.size, Alignment::Right, None)).dim(),
            style(&row.added).dim(),
        );
    }
}

/// Print each model as a short block of text with its preview drawn into a
/// reserved left gutter, separated by a rule.
fn print_model_cards(rows: &[ModelRow], term_width: usize, protocol: ImageProtocol) -> Result<()> {
    let mut stdout = std::io::stdout();
    let text_width = term_width.saturating_sub(CARD_GUTTER).max(20);
    let rule = style("\u{2500}".repeat(term_width.min(100)))
        .dim()
        .to_string();

    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            writeln!(stdout, "{rule}")?;
        }
        let thumbnail = row
            .preview
            .as_deref()
            .filter(|path| path.is_file())
            .and_then(load_thumbnail)
            // A model with no preview, or one whose file will not decode,
            // still gets a thumbnail so the list keeps its shape.
            .unwrap_or_else(missing_preview_art);
        let lines = card_lines(row, text_width);
        let height = lines.len().max(THUMB_CELLS_TALL as usize);

        // Reserve the rows first: the text goes down the right of the gutter,
        // with the border rule standing between the two columns.
        for offset in 0..height {
            let border = style('\u{2502}').dim();
            match lines.get(offset) {
                Some(line) => writeln!(
                    stdout,
                    "{:pad$}{border} {line}",
                    "",
                    pad = THUMB_CELLS_WIDE as usize + 1
                )?,
                None => writeln!(
                    stdout,
                    "{:pad$}{border}",
                    "",
                    pad = THUMB_CELLS_WIDE as usize + 1
                )?,
            }
        }

        // Step back over those rows and draw into the empty gutter, then
        // return to the end of the block.
        stdout.flush()?;
        let config = viuer::Config {
            absolute_offset: false,
            x: 0,
            y: -(height as i16),
            width: Some(THUMB_CELLS_WIDE),
            height: Some(THUMB_CELLS_TALL),
            restore_cursor: true,
            // Pinned, so viuer never probes for a protocol we ruled out.
            // Both off is what selects its half-block renderer.
            use_kitty: protocol == ImageProtocol::Kitty,
            use_iterm: protocol == ImageProtocol::ITerm,
            ..Default::default()
        };
        // A terminal that lied about its support must not abort the list.
        let _ = viuer::print(&thumbnail, &config);
        writeln!(stdout)?;
    }
    Ok(())
}

/// The text of one card: name, placement, provenance and location.
fn card_lines(row: &ModelRow, width: usize) -> Vec<String> {
    let joined = |parts: [&str; 3]| {
        parts
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" \u{b7} ")
    };
    let mut lines = vec![
        style(truncate_str(&row.name, width, "\u{2026}"))
            .bold()
            .to_string(),
        truncate_str(
            &joined([&row.model_type, &row.base_model, &row.size]),
            width,
            "\u{2026}",
        )
        .into_owned(),
    ];
    if !row.version.is_empty() {
        lines.push(
            style(truncate_str(&row.version, width, "\u{2026}"))
                .dim()
                .to_string(),
        );
    }
    lines.push(
        style(truncate_str(
            &format!("{}  {}", row.id, row.path),
            width,
            "\u{2026}",
        ))
        .dim()
        .to_string(),
    );
    lines.push(style(format!("added {}", row.added)).dim().to_string());
    lines
}

/// Pixel size of the one thumbnail shape every card uses.
fn thumbnail_pixels() -> (u32, u32) {
    (
        THUMB_CELLS_WIDE * CELL_PIXELS_WIDE,
        THUMB_CELLS_TALL * CELL_PIXELS_TALL,
    )
}

/// Decode a preview into that one shape.
///
/// Scaled to cover the box and centre-cropped, so previews of any proportion
/// come out at a single aspect ratio instead of each sizing its own box. The
/// format is sniffed from the bytes rather than the suffix: a preview's
/// extension comes from its source URL and can disagree with its content.
fn load_thumbnail(path: &Path) -> Option<DynamicImage> {
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    if image.width() == 0 || image.height() == 0 {
        return None;
    }
    let (wide, tall) = thumbnail_pixels();
    Some(image.resize_to_fill(wide, tall, image::imageops::FilterType::Triangle))
}

/// Stand-in drawn when a model has no preview, or its preview will not decode.
///
/// One pixel per character, scaled up with nearest-neighbour so it stays
/// deliberately pixelated rather than looking like a failed photograph.
#[rustfmt::skip]
const MISSING_PREVIEW_ART: [&str; 14] = [
    "##############",
    "#............#",
    "#...OO.......#",
    "#..OOOO......#",
    "#...OO.......#",
    "#............#",
    "#.........m..#",
    "#........mmm.#",
    "#...m...mmmmm#",
    "#..mmm.mmmmmm#",
    "#.mmmmmmmmmmm#",
    "#mmmmmmmmmmmm#",
    "##############",
    "##############",
];

fn missing_preview_art() -> DynamicImage {
    let side = MISSING_PREVIEW_ART.len() as u32;
    let mut art = image::RgbaImage::new(side, side);
    for (y, line) in MISSING_PREVIEW_ART.iter().enumerate() {
        for (x, cell) in line.chars().enumerate() {
            let colour = match cell {
                '#' => [88, 92, 104, 255],  // frame
                'O' => [222, 165, 74, 255], // sun
                'm' => [78, 124, 116, 255], // hills
                _ => [38, 42, 52, 255],     // sky
            };
            art.put_pixel(x as u32, y as u32, image::Rgba(colour));
        }
    }
    let (wide, tall) = thumbnail_pixels();
    DynamicImage::ImageRgba8(art).resize_to_fill(wide, tall, image::imageops::FilterType::Nearest)
}

#[allow(clippy::too_many_arguments)]
async fn run_templates(
    client: &mut IpcClient,
    query: Option<String>,
    kinds: Vec<String>,
    tasks: Vec<String>,
    families: Vec<String>,
    names: Vec<String>,
    comfortable_only: bool,
    include_api: bool,
    include_unrunnable: bool,
    refresh: bool,
    json: bool,
    yes: bool,
) -> Result<()> {
    let filter = TemplateFilter {
        text: query,
        kinds: kinds
            .iter()
            .map(|raw| parse_kind(raw))
            .collect::<Result<Vec<_>>>()?,
        tasks,
        families,
        names,
        include_api,
    };

    let data = ok_data(
        client
            .send(&Request::ListTemplates {
                filter,
                refresh,
                include_unrunnable,
            })
            .await?,
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let listing: TemplateListing =
        serde_json::from_value(data).context("parsing template listing")?;
    let mut bundles = listing.bundles;
    if comfortable_only {
        // Keep only bundles that fit outright; an unjudged bundle (no GPU
        // figure) stays, anything needing offload or unable to run does not.
        bundles.retain(|b| {
            matches!(b.feasibility, Some(Feasibility::Comfortable)) || b.feasibility.is_none()
        });
    }
    bundles.sort_by(|a, b| {
        a.feasibility
            .cmp(&b.feasibility)
            .then_with(|| a.template.title.cmp(&b.template.title))
    });

    match listing.gpu {
        Some(ref gpu) => println!(
            "GPU: {} ({} VRAM)",
            gpu.name,
            format_bytes(listing.vram_bytes.unwrap_or(gpu.vram_bytes))
        ),
        None => match listing.vram_bytes {
            Some(bytes) => println!("VRAM budget: {} (from config)", format_bytes(bytes)),
            None => println!("No GPU detected — feasibility is not being judged."),
        },
    }
    if listing.hidden_unrunnable > 0 {
        println!(
            "{} template(s) hidden: they cannot run on this GPU (--include-unrunnable to show).",
            listing.hidden_unrunnable
        );
    }

    if bundles.is_empty() {
        println!("No templates match that filter.");
        return Ok(());
    }

    let rows: Vec<PickRow> = bundles.iter().map(PickRow::from_bundle).collect();
    let selected: Vec<usize> = if yes || !std::io::stdout().is_terminal() {
        (0..bundles.len()).collect()
    } else {
        match multi_select(
            "Models to download (nothing is selected by default):",
            &rows,
        )? {
            Some(sel) => sel,
            None => {
                println!("Selection cancelled.");
                return Ok(());
            }
        }
    };

    if selected.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    let (queue, total_bytes) = queue_items_for(&bundles, &selected);

    println!(
        "\nQueueing {} file(s) from {} template(s), {} to download.",
        queue.len(),
        selected.len(),
        format_bytes(total_bytes)
    );

    let data = ok_data(client.send(&Request::AddDownloads { items: queue }).await?)?;
    let queued = data["queued"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("{queued} job(s) queued. Track them with `comfyui-dl status`.");
    if let Some(errors) = data["errors"].as_array().filter(|e| !e.is_empty()) {
        for err in errors {
            eprintln!("warning: {err}");
        }
    }
    Ok(())
}

fn parse_kind(raw: &str) -> Result<WorkloadKind> {
    let kind = WorkloadKind::from_template_type(raw);
    if kind == WorkloadKind::Unknown {
        bail!("unknown --type '{raw}' (expected image, video, audio, 3d or llm)");
    }
    Ok(kind)
}

/// One selectable template rendered as a set of aligned table cells.
struct PickRow {
    name: String,
    title: String,
    description: String,
    kind: &'static str,
    size: String,
    tier: &'static str,
    feasibility: Option<Feasibility>,
    files: String,
}

impl PickRow {
    fn from_bundle(bundle: &TemplateBundle) -> Self {
        let roles: Vec<&str> = bundle.models_by_role().keys().copied().collect();
        let tier = match bundle.feasibility {
            Some(Feasibility::Comfortable) => "fits",
            Some(Feasibility::CpuOffload) => "offload",
            Some(Feasibility::WontRun) => "won't run",
            None => "unknown",
        };
        Self {
            name: bundle.template.name.clone(),
            title: bundle.template.title.clone(),
            description: bundle.template.description.clone().unwrap_or_default(),
            kind: kind_label(bundle.template.kind),
            size: format_bytes(bundle.download_bytes),
            tier,
            feasibility: bundle.feasibility,
            files: format!("{} files: {}", bundle.models.len(), roles.join(", ")),
        }
    }
}

/// Pad `s` to exactly `width` display columns, ellipsizing only when it is
/// strictly wider (`pad_str` alone truncates exact-fit strings too).
fn fit(s: &str, width: usize, align: Alignment) -> String {
    if measure_text_width(s) > width {
        let truncated = truncate_str(s, width, "\u{2026}");
        pad_str(&truncated, width, align, None).into_owned()
    } else {
        pad_str(s, width, align, None).into_owned()
    }
}

/// Colour the feasibility cell by tier: green fits, yellow offload, red won't
/// run, dim unknown.
fn tier_cell(text: std::borrow::Cow<'_, str>, feasibility: Option<Feasibility>) -> String {
    let styled = style(text);
    match feasibility {
        Some(Feasibility::Comfortable) => styled.green(),
        Some(Feasibility::CpuOffload) => styled.yellow(),
        Some(Feasibility::WontRun) => styled.red(),
        None => styled.dim(),
    }
    .to_string()
}

/// Restore the terminal cursor no matter how the picker loop exits.
struct CursorGuard<'a>(&'a Term);

impl Drop for CursorGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.show_cursor();
    }
}

/// Scrolling multi-select picker. Nothing is selected by default; returns the
/// chosen indices, or `None` when the user cancels.
///
/// Return true if every character in `query` appears in `text` in order
/// (case-insensitive).
fn fuzzy_match(query: &str, text: &str) -> bool {
    let t = text.to_lowercase();
    let mut t_chars = t.chars();
    for qc in query.chars() {
        loop {
            match t_chars.next() {
                Some(tc) if tc == qc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// Return true if `query` fuzzy-matches any searchable field on `row`.
fn fuzzy_matches(query: &str, row: &PickRow) -> bool {
    if query.is_empty() {
        return true;
    }
    let haystacks = [&*row.name, &*row.title, &*row.description, &*row.files];
    haystacks.iter().any(|hay| fuzzy_match(query, hay))
}

/// Keys: arrows move, space or `x` toggle the row, `a` toggles select-all,
/// backspace clears every selection, `/` toggles fuzzy search, enter confirms,
/// esc/`q` cancels.
fn multi_select(prompt: &str, rows: &[PickRow]) -> Result<Option<Vec<usize>>> {
    let len = rows.len();
    if len == 0 {
        return Ok(Some(Vec::new()));
    }

    let term = Term::stderr();
    let (term_rows, term_cols) = term.size();
    let cols = term_cols as usize;
    // Reserve rows for the prompt, key legend, table header, possible search
    // line, footer and a spare.
    let page = (term_rows as usize).saturating_sub(6).clamp(1, len);

    // Column widths, sized to the widest cell (and the heading) in each column.
    let kind_w = rows.iter().map(|r| r.kind.len()).chain([4]).max().unwrap();
    let size_w = rows
        .iter()
        .map(|r| measure_text_width(&r.size))
        .chain([4])
        .max()
        .unwrap();
    let tier_w = rows.iter().map(|r| r.tier.len()).chain([3]).max().unwrap();
    // Fixed chrome: pointer, checkbox and the five 2-space gaps between cells.
    let fixed = 15 + kind_w + size_w + tier_w;
    let avail = cols.saturating_sub(fixed).max(24);
    let max_title = rows
        .iter()
        .map(|r| measure_text_width(&r.title))
        .chain([8])
        .max()
        .unwrap();
    let title_w = max_title.min(avail.saturating_sub(14)).max(8);
    let files_w = avail.saturating_sub(title_w).max(6);

    let mut checked = vec![false; len];
    let mut cursor = 0usize;
    let mut offset = 0usize;
    let mut search_mode = false;
    let mut query = String::new();
    let mut filtered_indices: Vec<usize> = (0..len).collect();

    let apply_filter = |query: &str, rows: &[PickRow]| -> Vec<usize> {
        (0..rows.len())
            .filter(|&i| fuzzy_matches(query, &rows[i]))
            .collect()
    };

    term.write_line(&style(prompt).bold().to_string())?;
    term.write_line(
        &style("  arrows move \u{b7} space/x toggle \u{b7} a select all \u{b7} backspace clear \u{b7} / search \u{b7} enter confirm \u{b7} esc cancel")
            .dim()
            .to_string(),
    )?;
    // Table header aligned under the data columns (7 cols of pointer + box).
    let header = format!(
        "       {}  {}  {}  {}  {}",
        pad_str("TEMPLATE", title_w, Alignment::Left, None),
        pad_str("TYPE", kind_w, Alignment::Left, None),
        pad_str("SIZE", size_w, Alignment::Right, None),
        pad_str("FIT", tier_w, Alignment::Left, None),
        "MODELS",
    );
    term.write_line(&style(header).bold().underlined().to_string())?;
    term.hide_cursor()?;
    let _guard = CursorGuard(&term);

    let mut drawn = 0usize;
    loop {
        let filt_len = filtered_indices.len();
        if filt_len > 0 {
            if cursor >= filt_len {
                cursor = filt_len - 1;
            }
            if cursor < offset {
                offset = cursor;
            } else if cursor >= offset + page {
                offset = cursor + 1 - page;
            }
        } else {
            cursor = 0;
            offset = 0;
        }

        if drawn > 0 {
            term.clear_last_lines(drawn)?;
        }
        drawn = 0;

        if filt_len > 0 {
            for (fi, &orig_i) in filtered_indices.iter().enumerate().skip(offset).take(page) {
                let row = &rows[orig_i];
                let on_cursor = fi == cursor;
                let pointer = if on_cursor {
                    style('>').cyan().bold().to_string()
                } else {
                    " ".to_string()
                };
                let checkbox = if checked[orig_i] {
                    style("[x]").green().to_string()
                } else {
                    style("[ ]").dim().to_string()
                };
                let title = fit(&row.title, title_w, Alignment::Left);
                let title = if on_cursor {
                    style(title).bold().to_string()
                } else {
                    title
                };
                let kind = style(pad_str(row.kind, kind_w, Alignment::Left, None))
                    .cyan()
                    .to_string();
                let size = style(pad_str(&row.size, size_w, Alignment::Right, None))
                    .dim()
                    .to_string();
                let tier = tier_cell(
                    pad_str(row.tier, tier_w, Alignment::Left, None),
                    row.feasibility,
                );
                let files = style(truncate_str(&row.files, files_w, "\u{2026}"))
                    .dim()
                    .to_string();
                term.write_line(&format!(
                    "{pointer} {checkbox}  {title}  {kind}  {size}  {tier}  {files}"
                ))?;
                drawn += 1;
            }
        } else {
            term.write_line(&style("  (no matches)").dim().to_string())?;
            drawn += 1;
        }

        if search_mode {
            let search_line = format!("> /{}", query);
            term.write_line(&style(search_line).cyan().to_string())?;
            drawn += 1;
        }

        let nsel = checked.iter().filter(|&&c| c).count();
        let visible = if filt_len < len {
            format!("{} of {len} visible", filt_len)
        } else {
            String::new()
        };
        let footer = if len > page {
            if filt_len > 0 {
                format!(
                    "  {nsel}/{len} selected \u{b7} showing {}-{} of {filt_len} {}",
                    offset + 1,
                    (offset + page).min(filt_len),
                    visible,
                )
            } else {
                format!("  {nsel}/{len} selected \u{b7} {visible}")
            }
        } else {
            format!("  {nsel}/{len} selected {visible}")
        };
        term.write_line(&style(footer.trim()).dim().to_string())?;
        drawn += 1;

        match term.read_key()? {
            Key::Char('/') if !search_mode => {
                search_mode = true;
                query.clear();
                filtered_indices = apply_filter(&query, rows);
                cursor = 0;
                offset = 0;
            }
            Key::Escape => {
                if search_mode {
                    search_mode = false;
                    query.clear();
                    filtered_indices = apply_filter(&query, rows);
                    cursor = 0;
                    offset = 0;
                } else {
                    term.clear_last_lines(drawn)?;
                    return Ok(None);
                }
            }
            Key::Enter => {
                if search_mode {
                    search_mode = false;
                } else {
                    term.clear_last_lines(drawn)?;
                    let sel = checked
                        .iter()
                        .enumerate()
                        .filter(|&(_, &c)| c)
                        .map(|(i, _)| i)
                        .collect();
                    return Ok(Some(sel));
                }
            }
            Key::Backspace => {
                if search_mode {
                    query.pop();
                    filtered_indices = apply_filter(&query, rows);
                    cursor = 0;
                    offset = 0;
                } else {
                    checked.iter_mut().for_each(|c| *c = false);
                }
            }
            Key::Char(' ') | Key::Char('x') | Key::Char('X') if filt_len > 0 => {
                let orig_i = filtered_indices[cursor];
                checked[orig_i] = !checked[orig_i];
            }
            Key::Char('a') | Key::Char('A') if !search_mode => {
                let all = checked.iter().all(|&c| c);
                checked.iter_mut().for_each(|c| *c = !all);
            }
            Key::Char(c) if search_mode => {
                query.push(c);
                filtered_indices = apply_filter(&query, rows);
                cursor = 0;
                offset = 0;
            }
            Key::ArrowUp if filt_len > 0 => {
                cursor = if cursor == 0 {
                    filt_len - 1
                } else {
                    cursor - 1
                }
            }
            Key::ArrowDown if filt_len > 0 => cursor = (cursor + 1) % filt_len,
            Key::CtrlC => {
                term.clear_last_lines(drawn)?;
                return Ok(None);
            }
            Key::Char('q') if !search_mode => {
                term.clear_last_lines(drawn)?;
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn kind_label(kind: WorkloadKind) -> &'static str {
    match kind {
        WorkloadKind::Image => "image",
        WorkloadKind::Video => "video",
        WorkloadKind::Audio => "audio",
        WorkloadKind::ThreeD => "3d",
        WorkloadKind::Llm => "llm",
        WorkloadKind::Unknown => "other",
    }
}

fn select_variant(files: &[FileVariantInfo]) -> Option<String> {
    if files.len() <= 1 {
        return files.first().map(|f| f.name.clone());
    }
    if std::io::stdout().is_terminal() {
        let items: Vec<String> = files.iter().map(format_variant).collect();
        match dialoguer::Select::new()
            .with_prompt("Multiple variants available. Select one to download")
            .items(&items)
            .default(0)
            .interact()
        {
            Ok(idx) => Some(files[idx].name.clone()),
            Err(_) => select_largest(files),
        }
    } else {
        select_largest(files)
    }
}

fn select_largest(files: &[FileVariantInfo]) -> Option<String> {
    files
        .iter()
        .max_by(|a, b| {
            a.size_kb
                .partial_cmp(&b.size_kb)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|f| f.name.clone())
}

fn format_variant(f: &FileVariantInfo) -> String {
    let size = format_bytes((f.size_kb * 1024.0) as u64);
    let mut parts = vec![f.name.clone(), format!("({size})")];
    if let Some(ref fmt) = f.format {
        parts.push(fmt.clone());
    }
    if let Some(ref s) = f.size {
        parts.push(s.clone());
    }
    if let Some(ref fp) = f.fp {
        parts.push(fp.clone());
    }
    if let Some(ref qt) = f.quant_type {
        parts.push(qt.clone());
    }
    parts.join(" | ")
}

/// Stores a credential in the Secret Service and drops any plaintext copy
/// left in `config.toml`.
async fn set_key(key: Option<String>, service: &str) -> Result<()> {
    let cred = secrets::parse_service(service)
        .with_context(|| format!("unknown service {service:?}; expected civitai or huggingface"))?;

    let raw = match key {
        Some(inline) => inline,
        None if std::io::stdin().is_terminal() => {
            let term = Term::stderr();
            term.write_str(&format!("Enter the {} key: ", cred.service_name()))?;
            term.read_secure_line()
                .context("reading the key from the terminal")?
        }
        None => {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("reading the key from stdin")?;
            line
        }
    };
    let secret = secrets::non_empty(&raw).context("the credential is empty")?;
    secrets::check_format(cred, &secret)?;
    match secrets::verify_online(cred, &secret).await {
        Ok(secrets::Verdict::Accepted) => {
            println!("{} accepted the credential.", cred.service_name());
        }
        Ok(secrets::Verdict::Rejected(status)) => bail!(
            "{} rejected the credential (HTTP {status}); it was not stored",
            cred.service_name()
        ),
        // Offline is not proof of a bad key, so store it and say so.
        Err(e) => eprintln!("warning: could not verify the credential, storing it anyway: {e:#}"),
    }

    let store = secrets::Store::open().await.with_context(|| {
        format!(
            "no keyring available to store {}; start a Secret Service provider (gnome-keyring, kwallet) or set {} in {}",
            cred.service_name(),
            cred.config_field(),
            Config::config_path().display()
        )
    })?;
    store.set(cred, &secret).await?;
    println!(
        "Stored the {} credential in the system keyring.",
        cred.service_name()
    );

    let mut config = Config::load()?;
    if config.forget_plaintext(cred)? {
        println!(
            "Removed the plaintext {} from {}.",
            cred.config_field(),
            Config::config_path().display()
        );
    }
    println!(
        "A running daemon that had no key picks it up on its next request; one that already had a key needs `systemctl --user restart comfyui-downloader`."
    );
    Ok(())
}

/// Assemble the queue for the selected templates.
///
/// One url in one role is one placement, however many templates ask for it, so
/// those requests collapse and their sizes are counted once. Every referencing
/// template's family labels are kept: a file two unrelated families both use
/// has no single family, and attribution must be able to see that.
fn queue_items_for(
    bundles: &[crate::templates::TemplateBundle],
    selected: &[usize],
) -> (Vec<QueueItem>, u64) {
    let mut position: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut queue: Vec<QueueItem> = Vec::new();
    let mut total_bytes: u64 = 0;
    for index in selected {
        let Some(bundle) = bundles.get(*index) else {
            continue;
        };
        for model in &bundle.models {
            let key = (model.url.clone(), model.role.clone());
            match position.get(&key) {
                Some(&at) => {
                    merge_families(
                        &mut queue[at].template_families,
                        &bundle.template.model_families,
                    );
                }
                None => {
                    total_bytes += model.size_bytes.unwrap_or(0);
                    position.insert(key, queue.len());
                    let mut template_families = Vec::new();
                    merge_families(&mut template_families, &bundle.template.model_families);
                    queue.push(QueueItem {
                        url: model.url.clone(),
                        model_type: Some(model.role.clone()),
                        family: None,
                        template_families,
                    });
                }
            }
        }
    }
    (queue, total_bytes)
}

fn merge_families(into: &mut Vec<String>, labels: &[String]) {
    for label in labels {
        if !into.contains(label) {
            into.push(label.clone());
        }
    }
    into.sort();
}

fn families(merge: bool, apply: bool, restore_removed: bool) -> Result<()> {
    let mut config = Config::load()?;
    if !merge {
        if config.model_families.aliases.is_empty() {
            println!("No model-family aliases configured.");
        } else {
            println!(
                "Model-family aliases ({}):",
                Config::config_path().display()
            );
            for (source_label, browsing_label) in &config.model_families.aliases {
                println!("  {source_label} -> {browsing_label}");
            }
        }
        config.model_families.alias_table()?;
        return Ok(());
    }

    let plan = config.preview_alias_merge(crate::placement::DEFAULT_FAMILY_ALIASES);
    if plan.additions.is_empty() && plan.conflicts.is_empty() && plan.previously_removed.is_empty()
    {
        println!("Nothing to merge: this release ships no aliases your config lacks.");
        return Ok(());
    }
    if !plan.additions.is_empty() {
        println!("Additions:");
        for (source_label, browsing_label) in &plan.additions {
            println!("  + {source_label} -> {browsing_label}");
        }
    }
    if !plan.conflicts.is_empty() {
        println!("Conflicts (your value is kept):");
        for conflict in &plan.conflicts {
            println!(
                "  ! {} -> {} (default is {})",
                conflict.source_label, conflict.user_value, conflict.default_value
            );
        }
    }
    if !plan.previously_removed.is_empty() {
        println!("Previously removed (restored only with --restore-removed):");
        for (source_label, browsing_label) in &plan.previously_removed {
            println!("  - {source_label} -> {browsing_label}");
        }
    }
    if !apply {
        println!("\nPreview only. Re-run with --merge --apply to write these changes.");
        return Ok(());
    }
    let restore = if restore_removed {
        crate::config::RestoreRemoved::Yes
    } else {
        crate::config::RestoreRemoved::No
    };
    config.apply_alias_merge(&plan, restore);
    config.model_families.alias_table()?;
    config.save()?;
    println!("\nMerged into {}.", Config::config_path().display());
    Ok(())
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();

    // SetKey runs without a daemon connection — it writes to the keyring.
    if let Some(Command::SetKey { key, service }) = cli.command {
        return set_key(key, &service).await;
    }

    // Aliases live in config.toml, which the CLI owns directly.
    if let Some(Command::Families {
        merge,
        apply,
        restore_removed,
    }) = cli.command
    {
        return families(merge, apply, restore_removed);
    }

    let config = Config::load()?;
    let mut client = IpcClient::connect(&config.daemon.socket_path).await?;

    // The template picker is interactive and needs several round trips, so it
    // runs its own request flow instead of the single request/response path.
    if let Some(Command::Templates {
        query,
        kind,
        task,
        model,
        name,
        comfortable_only,
        include_api,
        include_unrunnable,
        refresh,
        json,
        yes,
    }) = cli.command
    {
        return run_templates(
            &mut client,
            query,
            kind,
            task,
            model,
            name,
            comfortable_only,
            include_api,
            include_unrunnable,
            refresh,
            json,
            yes,
        )
        .await;
    }

    // `list` renders its own table and may need the enriched listing, so it
    // does not go through the single request/response path either.
    if let Some(Command::List {
        json,
        images,
        page,
        per_page,
        all,
    }) = cli.command
    {
        return run_list(&mut client, json, images, page, per_page, all).await;
    }

    let is_status = cli.command.is_none() || matches!(cli.command, Some(Command::Status));
    let is_doctor = matches!(cli.command, Some(Command::Doctor { .. }));

    let req = match cli.command {
        None | Some(Command::Status) => Request::GetStatus,
        Some(Command::Add {
            url,
            model_type,
            family,
        }) => {
            let preferred = match client
                .send(&Request::GetVersionInfo { url: url.clone() })
                .await
            {
                Ok(Response::Ok(data)) => serde_json::from_value::<VersionInfo>(data)
                    .ok()
                    .and_then(|v| select_variant(&v.files)),
                _ => None,
            };
            Request::AddDownload {
                url,
                model_type,
                preferred_file_name: preferred,
                family,
            }
        }
        Some(Command::Doctor { repair }) => Request::Diagnose { repair },
        Some(Command::Delete { id }) => Request::DeleteModel {
            id: resolve_model_id(&mut client, &id).await?,
        },
        Some(Command::CheckUpdates) => Request::CheckUpdates,
        Some(Command::Cancel { id }) => Request::Cancel {
            id: resolve_active_id(&mut client, &id).await?,
        },
        Some(Command::Updates) => Request::ListUpdates,
        Some(Command::DownloadVersion {
            model_id,
            version_id,
        }) => Request::DownloadVersion {
            model_id,
            version_id,
        },
        Some(Command::RedownloadMissing { all }) => Request::RedownloadMissing { all },
        Some(Command::SetKey { .. })
        | Some(Command::Templates { .. })
        | Some(Command::Families { .. })
        | Some(Command::List { .. }) => unreachable!(),
    };

    let is_updates = matches!(req, Request::ListUpdates);

    let response = client.send(&req).await?;

    if is_status {
        print_status(&response)?;
    } else if is_updates {
        print_updates(&response)?;
    } else if is_doctor {
        print_doctor(&response)?;
    } else {
        println!("{}", serde_json::to_string_pretty(&response)?);
    }
    Ok(())
}

fn print_doctor(response: &Response) -> Result<()> {
    let data = match response {
        Response::Ok(data) => data,
        Response::Err { message } => bail!("daemon error: {message}"),
    };
    let empty = Vec::new();
    let dangling = data["dangling"].as_array().unwrap_or(&empty);
    let duplicates = data["duplicate_paths"].as_array().unwrap_or(&empty);
    let untracked = data["untracked"].as_array().unwrap_or(&empty);

    if dangling.is_empty() && duplicates.is_empty() && untracked.is_empty() {
        println!("Catalog and models directory agree.");
    }

    if !dangling.is_empty() {
        println!(
            "{} catalog entr{} name a file that is not on disk:",
            dangling.len(),
            if dangling.len() == 1 { "y" } else { "ies" }
        );
        for job in dangling {
            let id = job["id"].as_str().unwrap_or("?");
            let path = job["dest_path"].as_str().unwrap_or("?");
            println!("  {} {}", &id[..8.min(id.len())], path);
        }
        println!("  `comfyui-dl redownload-missing --all` would fetch these again.");
    }

    if !duplicates.is_empty() {
        println!(
            "\n{} path(s) named by more than one entry:",
            duplicates.len()
        );
        for pair in duplicates {
            let path = pair[0].as_str().unwrap_or("?");
            let count = pair[1].as_u64().unwrap_or(0);
            println!("  {count} entries -> {path}");
        }
    }

    if !untracked.is_empty() {
        println!("\n{} model file(s) no entry names:", untracked.len());
        for path in untracked {
            println!("  {}", path.as_str().unwrap_or("?"));
        }
    }

    if let Some(credentials) = data["credentials"].as_array() {
        println!("\nCredentials in use by the daemon:");
        for entry in credentials {
            let mark = match entry["ok"].as_bool() {
                Some(true) => "ok ",
                Some(false) => "BAD",
                None => " - ",
            };
            println!(
                "  [{mark}] {}: {}",
                entry["service"].as_str().unwrap_or("?"),
                entry["detail"].as_str().unwrap_or("?")
            );
            if entry["ok"].as_bool() == Some(false) {
                println!(
                    "        fix with `comfyui-dl set-key --service {}`, then restart the daemon",
                    entry["service"].as_str().unwrap_or("civitai")
                );
            }
        }
    }

    match &data["repaired"] {
        serde_json::Value::Null => {
            if !dangling.is_empty() || !duplicates.is_empty() {
                println!(
                    "\nRe-run with --repair to collapse duplicates and drop entries that others already cover."
                );
            }
        }
        repaired => {
            println!(
                "\nRepaired: {} duplicate entr{} collapsed, {} dangling entr{} dropped.",
                repaired["removed_duplicates"].as_u64().unwrap_or(0),
                if repaired["removed_duplicates"].as_u64() == Some(1) {
                    "y"
                } else {
                    "ies"
                },
                repaired["removed_dangling"].as_u64().unwrap_or(0),
                if repaired["removed_dangling"].as_u64() == Some(1) {
                    "y"
                } else {
                    "ies"
                },
            );
            if let Some(unresolved) = repaired["unresolved"].as_array().filter(|u| !u.is_empty()) {
                println!(
                    "{} entr{} kept: nothing else accounts for that model.",
                    unresolved.len(),
                    if unresolved.len() == 1 { "y" } else { "ies" }
                );
            }
        }
    }
    Ok(())
}

fn print_status(response: &Response) -> Result<()> {
    let data = match response {
        Response::Ok(data) => data,
        Response::Err { message } => bail!("daemon error: {message}"),
    };

    let active = data["active"].as_array();
    let queued_jobs = data["queued"].as_array();
    let queued = queued_jobs.map(|a| a.len() as u64).unwrap_or(0);
    let free_bytes = data["free_bytes"].as_u64().unwrap_or(0);

    if let Some(jobs) = active {
        if jobs.is_empty() {
            println!("No active downloads.");
        } else {
            println!(
                "{}",
                if jobs.len() == 1 {
                    "Downloading:".to_string()
                } else {
                    format!("Downloading ({}):", jobs.len())
                }
            );
            println!();
            for job in jobs {
                print_active_job(job);
            }
        }
    } else {
        println!("No active downloads.");
    }

    if queued > 0 {
        println!("Queued: {queued}");
        if let Some(jobs) = queued_jobs {
            for job in jobs {
                print_queued_job(job);
            }
        }
        println!();
    }

    println!("Free disk space: {}", format_bytes(free_bytes));

    Ok(())
}

fn print_updates(response: &Response) -> Result<()> {
    let data = match response {
        Response::Ok(data) => data,
        Response::Err { message } => bail!("daemon error: {message}"),
    };

    let updates = data.as_array();
    match updates {
        Some(items) if items.is_empty() => {
            println!("All models are up to date.");
        }
        Some(items) => {
            println!("{} update(s) available:\n", items.len());
            for item in items {
                let model_id = item["model_id"].as_u64().unwrap_or(0);
                let current_version = item["version_id"].as_u64().unwrap_or(0);
                let new_version = item["available_version_id"].as_u64().unwrap_or(0);
                let version_name = item["available_version_name"].as_str().unwrap_or("unknown");
                let model_type = item["model_type"].as_str().unwrap_or("?");
                let dest_path = item["dest_path"].as_str();

                let filename = dest_path.and_then(|p| p.rsplit('/').next()).unwrap_or("?");

                println!("  {filename}  [{model_type}]");
                println!("    version {current_version} \u{2192} {new_version} ({version_name})");
                println!("    comfyui-dl download-version {model_id} {new_version}");
                println!();
            }
        }
        None => {
            println!("All models are up to date.");
        }
    }

    Ok(())
}

fn print_active_job(job: &serde_json::Value) {
    let id = job["id"].as_str().unwrap_or("");
    let name = job["model_name"].as_str().unwrap_or("Unknown model");
    let bytes_received = job["bytes_received"].as_u64().unwrap_or(0);
    let total_bytes = job["total_bytes"].as_u64();
    let dest_path = job["dest_path"].as_str();
    let model_type = job["model_type"].as_str();
    let download_reason = job["download_reason"].as_str();
    let started_at = job["started_at"]
        .as_str()
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());

    print!("  {}  {name}", short_id(id));
    if let Some(mt) = model_type {
        print!("  [{mt}]");
    }
    println!();

    if let Some(total) = total_bytes {
        let pct = if total > 0 {
            (bytes_received as f64 / total as f64 * 100.0) as u64
        } else {
            0
        };
        let bar = progress_bar(pct, 30);
        println!(
            "  {bar} {pct:>3}%  ({} / {})",
            format_bytes(bytes_received),
            format_bytes(total),
        );

        if let Some(started) = started_at {
            let elapsed = Utc::now().signed_duration_since(started);
            let elapsed_secs = elapsed.num_seconds().max(1) as f64;
            if bytes_received > 0 && total > bytes_received {
                let remaining_bytes = total - bytes_received;
                let speed = bytes_received as f64 / elapsed_secs;
                let eta_secs = (remaining_bytes as f64 / speed) as u64;
                println!(
                    "  ETA: {}  ({}/s)",
                    format_duration(eta_secs),
                    format_bytes(speed as u64),
                );
            }
        }
    } else {
        println!("  {} downloaded", format_bytes(bytes_received));
    }

    if let Some(path) = dest_path {
        println!("  Path: {path}");
    }

    if download_reason == Some("update_available") {
        println!("  \u{2191} Upgrade from previous version");
    }

    println!();
}

fn print_queued_job(job: &serde_json::Value) {
    let id = job["id"].as_str().unwrap_or("");
    let url = job["url"].as_str().unwrap_or("?");
    let model_type = job["model_type"].as_str();
    let download_reason = job["download_reason"].as_str();

    print!("  \u{23f3} {}  {url}", short_id(id));
    if let Some(mt) = model_type {
        print!("  [{mt}]");
    }
    if download_reason == Some("update_available") {
        print!("  (upgrade)");
    }
    println!();
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Resolve a user-provided ID (full UUID or unique prefix) against the active and
/// queued jobs reported by `GetStatus`.
async fn resolve_active_id(client: &mut IpcClient, input: &str) -> Result<Uuid> {
    if let Ok(uuid) = Uuid::parse_str(input) {
        return Ok(uuid);
    }

    let data = ok_data(client.send(&Request::GetStatus).await?)?;
    let candidates = ["active", "queued"]
        .iter()
        .filter_map(|k| data[k].as_array())
        .flatten()
        .filter_map(|job| job["id"].as_str());
    match_prefix(input, candidates, "active or queued job")
}

/// Resolve a user-provided ID (full UUID or unique prefix) against the catalogued
/// models reported by `ListModels`.
async fn resolve_model_id(client: &mut IpcClient, input: &str) -> Result<Uuid> {
    if let Ok(uuid) = Uuid::parse_str(input) {
        return Ok(uuid);
    }

    let data = ok_data(client.send(&Request::ListModels).await?)?;
    let candidates = data
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str());
    match_prefix(input, candidates, "catalogued model")
}

fn ok_data(response: Response) -> Result<serde_json::Value> {
    match response {
        Response::Ok(data) => Ok(data),
        Response::Err { message } => bail!("daemon error: {message}"),
    }
}

fn match_prefix<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    kind: &str,
) -> Result<Uuid> {
    let matches: Vec<Uuid> = candidates
        .into_iter()
        .filter(|s| s.starts_with(input))
        .filter_map(|s| Uuid::parse_str(s).ok())
        .collect();
    match matches.len() {
        0 => bail!("no {kind} matches id '{input}'"),
        1 => Ok(matches[0]),
        n => bail!("id prefix '{input}' is ambiguous ({n} matches); use a longer prefix"),
    }
}

fn progress_bar(pct: u64, width: usize) -> String {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width - filled;
    format!(
        "[{}{}]",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;

    if hours > 0 {
        format!("{hours}h {mins:02}m {s:02}s")
    } else if mins > 0 {
        format!("{mins}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod comfyui_test_support {
        use crate::templates::{TemplateBundle, TemplateEntry, TemplateModel};
        use crate::vram::{VramRequirement, WorkloadKind};

        pub fn bundle(families: &[&str], models: &[(&str, &str)]) -> TemplateBundle {
            TemplateBundle {
                template: TemplateEntry {
                    name: "t".to_string(),
                    title: "T".to_string(),
                    description: None,
                    tags: Vec::new(),
                    model_families: families.iter().map(|f| (*f).to_string()).collect(),
                    category: "Image".to_string(),
                    kind: WorkloadKind::Image,
                    total_size_bytes: None,
                    tutorial_url: None,
                    is_api: false,
                },
                models: models
                    .iter()
                    .map(|(url, role)| TemplateModel {
                        file_name: "m.safetensors".to_string(),
                        url: (*url).to_string(),
                        role: (*role).to_string(),
                        size_bytes: Some(10),
                        sha256: None,
                    })
                    .collect(),
                requirement: VramRequirement::default(),
                feasibility: None,
                download_bytes: 10,
            }
        }
    }

    #[test]
    fn test_a_single_family_template_labels_its_files() {
        let bundles = vec![comfyui_test_support::bundle(
            &["Flux.1 D"],
            &[("https://example.invalid/a.safetensors", "vae")],
        )];

        let (items, _) = queue_items_for(&bundles, &[0]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].template_families, vec!["Flux.1 D".to_string()]);
    }

    #[test]
    fn test_a_file_shared_by_two_families_carries_both_labels() {
        let shared = "https://example.invalid/shared_encoder.safetensors";
        let bundles = vec![
            comfyui_test_support::bundle(&["Flux.1 D"], &[(shared, "text_encoders")]),
            comfyui_test_support::bundle(&["SD1.5"], &[(shared, "text_encoders")]),
        ];

        let (items, bytes) = queue_items_for(&bundles, &[0, 1]);

        assert_eq!(items.len(), 1, "one url in one role is one placement");
        assert_eq!(
            items[0].template_families,
            vec!["Flux.1 D".to_string(), "SD1.5".to_string()],
            "both templates' labels survive so attribution can abstain"
        );
        assert_eq!(bytes, 10, "the shared file is counted once");
    }

    #[test]
    fn test_one_file_wanted_in_two_roles_stays_two_items() {
        let url = "https://example.invalid/m.safetensors";
        let bundles = vec![comfyui_test_support::bundle(
            &["Flux.1 D"],
            &[(url, "vae"), (url, "loras")],
        )];

        let (items, _) = queue_items_for(&bundles, &[0]);

        assert_eq!(items.len(), 2);
    }

    fn enriched(name: Option<&str>, dest: Option<&str>) -> EnrichedModel {
        EnrichedModel {
            id: Uuid::parse_str("3f2504e0-4f89-41d3-9a0c-0305e82c3301").unwrap(),
            url: "https://example.invalid/m.safetensors".to_string(),
            model_id: None,
            version_id: None,
            model_type: Some("loras".to_string()),
            dest_path: dest.map(String::from),
            created_at: "2026-01-02T03:04:05Z".parse().unwrap(),
            updated_at: "2026-01-02T03:04:05Z".parse().unwrap(),
            model_name: name.map(String::from),
            version_name: None,
            base_model: Some("Flux.1 D".to_string()),
            preview_path: None,
            preview_nsfw_level: None,
            file_size: Some(2048),
            sha256: None,
        }
    }

    #[test]
    fn test_a_row_prefers_the_sidecar_name() {
        let row = ModelRow::from_model(&enriched(Some("Pretty Name"), Some("/m/a.safetensors")));

        assert_eq!(row.name, "Pretty Name");
        assert_eq!(row.id, "3f2504e0");
        assert_eq!(row.size, "2 KiB");
        assert_eq!(row.added, "2026-01-02");
    }

    #[test]
    fn test_a_row_without_metadata_falls_back_to_the_filename() {
        let row = ModelRow::from_model(&enriched(None, Some("/models/loras/a.safetensors")));

        assert_eq!(row.name, "a.safetensors");
    }

    #[test]
    fn test_a_row_with_neither_name_nor_path_falls_back_to_the_url() {
        let row = ModelRow::from_model(&enriched(None, None));

        assert_eq!(row.name, "https://example.invalid/m.safetensors");
    }

    #[test]
    fn test_the_name_column_absorbs_the_leftover_width() {
        let rows = vec![ModelRow::from_model(&enriched(
            Some("a name that is comfortably long"),
            Some("/m/a.safetensors"),
        ))];

        let wide = columns_for(&rows, 200);
        let narrow = columns_for(&rows, 60);

        assert_eq!(wide.name, "a name that is comfortably long".len());
        assert!(
            narrow.name < wide.name,
            "a narrow terminal must squeeze the name column, got {}",
            narrow.name
        );
        assert_eq!(narrow.kind, "loras".len());
        assert_eq!(narrow.base, "Flux.1 D".len());
    }

    #[test]
    fn test_the_name_column_keeps_a_floor_on_a_tiny_terminal() {
        let rows = vec![ModelRow::from_model(&enriched(
            Some("a name that is comfortably long"),
            None,
        ))];

        assert_eq!(columns_for(&rows, 1).name, 12);
    }

    #[test]
    fn test_the_placeholder_matches_the_one_thumbnail_shape() {
        let art = missing_preview_art();

        assert_eq!((art.width(), art.height()), thumbnail_pixels());
    }

    #[test]
    fn test_the_placeholder_art_is_square_and_fully_specified() {
        let side = MISSING_PREVIEW_ART.len();
        for line in MISSING_PREVIEW_ART {
            assert_eq!(
                line.chars().count(),
                side,
                "every row must be as wide as the art is tall: {line:?}"
            );
        }
    }

    #[test]
    fn test_a_page_defaults_to_the_first_ten() {
        let page = Page::of(300, 1, DEFAULT_PAGE_SIZE, false).unwrap();

        assert_eq!((page.start, page.end), (0, 10));
        assert_eq!(page.next_page, Some(2));
        assert_eq!(
            page.caption().unwrap(),
            "showing 1-10 of 300 \u{b7} --page 2 for the next"
        );
    }

    #[test]
    fn test_the_last_page_is_short_and_offers_no_next() {
        let page = Page::of(25, 3, 10, false).unwrap();

        assert_eq!((page.start, page.end), (20, 25));
        assert_eq!(page.next_page, None);
        assert!(page.caption().unwrap().contains("showing 21-25 of 25"));
    }

    #[test]
    fn test_all_shows_everything_and_captions_nothing() {
        let page = Page::of(300, 1, DEFAULT_PAGE_SIZE, true).unwrap();

        assert_eq!((page.start, page.end), (0, 300));
        assert_eq!(page.caption(), None, "nothing is being paged through");
    }

    #[test]
    fn test_a_page_past_the_end_is_refused_with_the_count() {
        let err = Page::of(25, 4, 10, false).unwrap_err().to_string();

        assert!(err.contains("25 model(s) make 3 page(s)"), "got {err:?}");
    }

    #[test]
    fn test_page_zero_is_refused() {
        assert!(Page::of(25, 0, 10, false).is_err());
    }

    #[test]
    fn test_a_catalog_shorter_than_one_page_is_not_captioned() {
        let page = Page::of(4, 1, 10, false).unwrap();

        assert_eq!((page.start, page.end), (0, 4));
        assert_eq!(page.caption(), None);
    }

    #[test]
    fn test_cards_never_exceed_the_width_they_are_given() {
        let row = ModelRow::from_model(&enriched(
            Some("an extremely long model name that would overflow a narrow terminal"),
            Some("/a/very/deep/models/tree/loras/some-long-file-name.safetensors"),
        ));

        for line in card_lines(&row, 30) {
            assert!(
                measure_text_width(&line) <= 30,
                "card line overflows: {line:?}"
            );
        }
    }

    #[test]
    fn test_never_suppresses_thumbnails_without_probing_the_terminal() {
        assert_eq!(thumbnail_protocol(ImageMode::Never), None);
    }

    #[test]
    fn test_a_piped_run_draws_no_images_even_when_forced() {
        // Tests capture stdout, so this run is never a terminal.
        assert_eq!(thumbnail_protocol(ImageMode::Always), None);
        assert_eq!(thumbnail_protocol(ImageMode::Auto), None);
    }

    #[test]
    fn test_an_unremarkable_terminal_is_never_asked_to_answer_the_handshake() {
        assert!(!announces_kitty(false, Some("xterm-256color"), None));
        assert!(!announces_kitty(false, Some("screen"), Some("herdr")));
        assert!(!announces_kitty(false, None, None));
    }

    #[test]
    fn test_a_terminal_that_names_itself_kitty_is_probed() {
        assert!(announces_kitty(false, Some("xterm-kitty"), None));
        assert!(announces_kitty(true, Some("xterm-256color"), None));
        assert!(announces_kitty(
            false,
            Some("xterm-ghostty"),
            Some("ghostty")
        ));
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1 KiB");
        assert_eq!(format_bytes(1_048_576), "1.0 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GiB");
        assert_eq!(format_bytes(5_368_709_120), "5.00 GiB");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3661), "1h 01m 01s");
    }

    #[test]
    fn test_progress_bar() {
        let bar = progress_bar(50, 10);
        assert_eq!(
            bar,
            "[\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}]"
        );

        let bar_full = progress_bar(100, 5);
        assert_eq!(bar_full, "[\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}]");

        let bar_empty = progress_bar(0, 5);
        assert_eq!(bar_empty, "[\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}]");
    }

    #[test]
    fn test_select_variant_single_file() {
        let files = vec![FileVariantInfo {
            name: "model.safetensors".to_string(),
            size_kb: 1000.0,
            primary: Some(true),
            format: Some("SafeTensor".to_string()),
            size: None,
            fp: None,
            quant_type: None,
            component_type: None,
        }];
        assert_eq!(
            select_variant(&files),
            Some("model.safetensors".to_string())
        );
    }

    #[test]
    fn test_select_largest_picks_max_size() {
        let files = vec![
            FileVariantInfo {
                name: "small.safetensors".to_string(),
                size_kb: 1000.0,
                primary: Some(true),
                format: None,
                size: None,
                fp: None,
                quant_type: None,
                component_type: None,
            },
            FileVariantInfo {
                name: "large.safetensors".to_string(),
                size_kb: 5000.0,
                primary: Some(false),
                format: None,
                size: None,
                fp: None,
                quant_type: None,
                component_type: None,
            },
        ];
        assert_eq!(
            select_largest(&files),
            Some("large.safetensors".to_string())
        );
    }

    #[test]
    fn test_format_variant_shows_all_metadata() {
        let f = FileVariantInfo {
            name: "model.safetensors".to_string(),
            size_kb: 1024.0,
            primary: Some(true),
            format: Some("SafeTensor".to_string()),
            size: Some("pruned".to_string()),
            fp: Some("fp16".to_string()),
            quant_type: None,
            component_type: None,
        };
        let s = format_variant(&f);
        assert!(s.contains("model.safetensors"));
        assert!(s.contains("SafeTensor"));
        assert!(s.contains("pruned"));
        assert!(s.contains("fp16"));
    }
}
