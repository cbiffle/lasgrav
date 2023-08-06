use std::{path::PathBuf, fmt::Display};

use anyhow::{bail, Result, Context};
use clap::{Parser, ValueEnum};
use image::imageops::FilterType;

/// A very simple tool for producing laser engraver toolpaths from raster
/// images.
#[derive(Parser)]
struct Lasgrav {
    image: PathBuf,

    /// Input file resolution specified in dots-per-inch. This determines the
    /// transformation from the input sample grid to machine space. You can
    /// alter this value to scale the image, if desired.
    ///
    /// This argument is in US conventional units, rather than SI, due to
    /// industry convention.
    #[clap(short, long, default_value_t = 300., help_heading = "Import Options")]
    dpi: f64,
    /// Pixel luminance level to treat as "white" or "off." Any pixel with this
    /// value or greater (after the conversion to grayscale) will be treated as
    /// "not engraved," and any pixel with this value or lower will be engraved.
    #[clap(short, long, default_value_t = 128, help_heading = "Import Options")]
    threshold: u8,
    /// Interpolation method to use when sampling image; only really matters if
    /// the output lines don't exactly map to pixels.
    #[clap(short, long, default_value_t = Interp::Gaussian, help_heading = "Import Options")]
    interp: Interp,

    /// Lines per mm in the output engraving.
    #[clap(short, long, default_value_t = 8, help_heading = "Output Options")]
    lines_per_mm: u32,
    /// Feed rate, in mm/min.
    #[clap(short, long, default_value_t = 1000, help_heading = "Output Options")]
    feed: u32,
    /// Laser power for "on" sections.
    #[clap(short, long, default_value_t = 1000, help_heading = "Output Options")]
    power: u32,
    /// Horizontal motion strategy. Generally bidirectional movement is fastest,
    /// but if your machine has backlash on the X axis, unidirectional movement
    /// may produce better output.
    #[clap(short, long, default_value_t = HMotion::Bi, help_heading = "Output Options")]
    motion: HMotion,
    /// Force precision of numbers in GCode (decimal places of fractional
    /// millimeters). By default this is computed from the machine step size.
    /// You probably only want to override this to compare the output to another
    /// generator with a specific precision setting.
    #[clap(long, help_heading = "Output Options")]
    precision: Option<u8>,
    /// Decreases horizontal resolution to match the lines-per-mm setting.
    /// Generally this is a bad idea. It's mostly useful to compare the output
    /// to another generator that behaves this way.
    #[clap(long, help_heading = "Output Options")]
    quantize_horizontal: bool,

    /// Steps per mm in the output machine. Currently this assumes that there
    /// are an integral number of steps per millimeter.
    #[clap(short, long, default_value_t = 160, help_heading = "Machine Options")]
    steps_per_mm: u32,

    /// Path to write the intermediate processed image after thresholding, for
    /// checking the results.
    #[clap(long, help_heading = "Debugging Tools")]
    save_intermediate: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Lasgrav::parse();

    // Basic checks
    if args.dpi <= 0. {
        bail!("DPI must be positive, but was set as: {}", args.dpi);
    }
    if args.steps_per_mm == 0 {
        bail!("steps/mm must not be zero.");
    }
    if args.lines_per_mm == 0 {
        bail!("lines/mm must not be zero.");
    }

    // Check that we can actually produce the desired lines/mm.
    if args.steps_per_mm % args.lines_per_mm != 0 {
        bail!("can't evenly divide {} steps/mm into {} lines",
            args.steps_per_mm, args.lines_per_mm);
    }

    let steps_per_line = args.steps_per_mm / args.lines_per_mm;

    let dpmm = args.dpi / 25.4;

    let image = image::io::Reader::open(&args.image)
        .with_context(|| format!("loading image file {}", args.image.display()))?
        .decode()
        .with_context(|| format!("decoding image file {}", args.image.display()))?;

    // Centralize the conversion to 8-bit luma until we have a more interesting
    // transfer function.
    let image = image.into_luma8();

    let w = image.width() as f64 / dpmm;
    let h = image.height() as f64 / dpmm;
    eprintln!("image size: {w:.3} mm x {h:.3} mm");

    let ws = (w * args.steps_per_mm as f64).ceil() as u64;
    let hs = (h * args.steps_per_mm as f64).ceil() as u64;
    eprintln!("in steps: {ws} x {hs}");

    let line_count = hs / u64::from(steps_per_line); // deliberate round down
    let dpl = dpmm / args.lines_per_mm as f64;
    eprintln!("engraving consists of {line_count} lines ({dpl:.03} input pixels per line)");

    let width = if args.quantize_horizontal {
        (ws / u64::from(steps_per_line)) as u32
    } else {
        image.width()
    };

    eprintln!("scaling vertically{} using {}",
        if args.quantize_horizontal { " and horizontally" } else { "" },
        args.interp);
    let resized = image::imageops::resize(&image, width, line_count as u32, match args.interp {
        Interp::Nearest => FilterType::Nearest,
        Interp::Gaussian => FilterType::Gaussian,
        Interp::Lanczos3 => FilterType::Lanczos3,
        Interp::Cubic => FilterType::CatmullRom,
    });

    if let Some(p) = args.save_intermediate {
        resized.save(&p)
            .with_context(|| format!("writing intermediate output to {}", p.display()))?;
    }

    eprint!("computing thresholded image spans...");

    let mut rows = vec![];
    for y in 0..line_count as u32 {
        let mut spans = vec![];
        let mut on = None;
        for x in 0..resized.width() {
            let p = resized.get_pixel(x, y).0[0] < args.threshold;
            if p && on.is_none() {
                on = Some(x);
            } else if !p && on.is_some() {
                let start = on.unwrap();
                // Spans are recorded _inclusive_ of the ending coordinate,
                // because we're going to etch a line from the leftmost edge of
                // the start to the leftmost edge of the end.
                spans.push((start, x));
                on = None;
            }
        }

        if let Some(start) = on {
            spans.push((start, resized.width()));
        }

        // Flip y coordinate
        rows.push((line_count as u32 - 1 - y, spans));
    }
    // Scan bottom-up in flipped Y coordinate.
    rows.reverse();

    eprintln!("done.");

    let dp = if let Some(p) = args.precision {
        eprintln!("forcing precision to {p} decimal places");
        p as usize
    } else {
        let x = format!("{}", 1. / args.steps_per_mm as f64).len() - 2;
        eprintln!("{} steps/mm requires at most {x} decimal places", args.steps_per_mm);
        x
    };
    let sdp = 10_f64.powi(dp as i32);
    let round = |f: f64| (f * sdp).round() / sdp;

    print!("G90\r\n");

    print!("G0 X0 Y0 F{}\r\n", args.feed);
    print!("M3 S0\r\n");
    let mm_per_line = 1. / args.lines_per_mm as f64;
    let half_line = mm_per_line / 2.;
    let mm_per_pixel = if args.quantize_horizontal {
        mm_per_line
    } else {
        1. / dpmm
    };
    let on = args.power;
    let mut odd = false;
    for (y, mut spans) in rows {
        if spans.is_empty() {
            continue;
        }

        let rtl = args.motion == HMotion::Bi && odd;

        if rtl {
            spans.reverse();
        }

        print!("( row {y}: {} )\r\n", if rtl { "<-" } else { "-> "});

        let yc = round(y as f64 * mm_per_line + half_line);
        for (sx, ex) in spans {
            let sxc = round(sx as f64 * mm_per_pixel);
            let exc = round(ex as f64 * mm_per_pixel);
            if rtl {
                print!("G0 X{exc} Y{yc} S0\r\n");
                print!("G1 X{sxc} S{on}\r\n");
            } else {
                print!("G0 X{sxc} Y{yc} S0\r\n");
                print!("G1 X{exc} S{on}\r\n");
            }
        }

        // Note: we're maintaining the odd flag manually instead of computing it
        // from the LSB of the y coordinate because we want to handle skipped
        // rows correctly.
        odd = !odd;
    }
    print!("M5\r\n");

    Ok(())
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Interp {
    Nearest,
    Gaussian,
    Lanczos3,
    Cubic,
}

impl Display for Interp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nearest => f.write_str("nearest"),
            Self::Gaussian => f.write_str("gaussian"),
            Self::Lanczos3 => f.write_str("lanczos3"),
            Self::Cubic => f.write_str("cubic"),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum HMotion {
    Uni,
    Bi,
}

impl Display for HMotion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uni => f.write_str("uni"),
            Self::Bi => f.write_str("bi"),
        }
    }
}
