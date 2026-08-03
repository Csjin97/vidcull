#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{self, BufRead, Write};

use ffmpeg_next as ff;

struct Decoder {
    ictx: ff::format::context::Input,
    decoder: ff::decoder::Video,
    scaler: ff::software::scaling::Context,
    vindex: usize,
    tb_num: f64,
    tb_den: f64,
    width: u32,
    height: u32,
}

impl Decoder {
    fn open(path: &str) -> Result<Self, ff::Error> {
        let ictx = ff::format::input(&path)?;
        let stream = ictx
            .streams()
            .best(ff::media::Type::Video)
            .ok_or(ff::Error::StreamNotFound)?;
        let vindex = stream.index();
        let tb = stream.time_base();
        let params = stream.parameters();
        let mut cctx = ff::codec::context::Context::from_parameters(params)?;
        unsafe {
            let p = cctx.as_mut_ptr();
            (*p).thread_count = 0;
            (*p).thread_type = ff::ffi::FF_THREAD_FRAME | ff::ffi::FF_THREAD_SLICE;
        }
        let decoder = cctx.decoder().video()?;
        let (width, height) = (decoder.width(), decoder.height());
        let scaler = ff::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ff::format::Pixel::GRAY8,
            width,
            height,
            ff::software::scaling::Flags::BILINEAR,
        )?;
        Ok(Self {
            ictx,
            decoder,
            scaler,
            vindex,
            tb_num: tb.numerator() as f64,
            tb_den: tb.denominator() as f64,
            width,
            height,
        })
    }

    fn gray_frame(&mut self, ts_ms: u64) -> Option<Vec<u8>> {
        let ts = ts_ms as f64 / 1000.0;
        let ts_avtb = (ts * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
        self.ictx.seek(ts_avtb, ..ts_avtb).ok()?;
        self.decoder.flush();
        let target_pts = (ts * self.tb_den / self.tb_num) as i64;

        let mut frame = ff::frame::Video::empty();
        let mut packets = self.ictx.packets();
        loop {
            let (stream, packet) = packets.next()?;
            if stream.index() != self.vindex {
                continue;
            }
            if self.decoder.send_packet(&packet).is_err() {
                continue;
            }
            while self.decoder.receive_frame(&mut frame).is_ok() {
                if frame.pts().unwrap_or(0) >= target_pts {
                    let mut gray = ff::frame::Video::empty();
                    self.scaler.run(&frame, &mut gray).ok()?;
                    return Some(pack_gray(&gray, self.width as usize, self.height as usize));
                }
            }
        }
    }
}

fn pack_gray(gray: &ff::frame::Video, width: usize, height: usize) -> Vec<u8> {
    let stride = gray.stride(0);
    let data = gray.data(0);
    let mut buf = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = y * stride;
        buf.extend_from_slice(&data[row..row + width]);
    }
    buf
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or(
        "usage: vidcull-decode-sidecar <file> [ts_ms,ts_ms,...]  (timestamps on argv[2] \
         comma-list, or stdin one-per-line)",
    )?;

    let timestamps: Vec<u64> = match std::env::args().nth(2) {
        Some(arg) => arg
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::parse::<u64>)
            .collect::<Result<_, _>>()?,
        None => {
            let stdin = io::stdin();
            let mut v = Vec::new();
            for line in stdin.lock().lines() {
                let line = line?;
                let s = line.trim();
                if !s.is_empty() {
                    v.push(s.parse::<u64>()?);
                }
            }
            v
        }
    };

    ff::init()?;
    let mut decoder = Decoder::open(&path)?; 

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    writeln!(
        out,
        "VIDCULL-SIDECAR-1 {} {} {}",
        decoder.width,
        decoder.height,
        timestamps.len()
    )?;

    for &ts in &timestamps {
        match decoder.gray_frame(ts) {
            Some(buf) if buf.len() == (decoder.width as usize * decoder.height as usize) => {
                out.write_all(&[1u8])?;
                out.write_all(&buf)?;
            }
            _ => {
                out.write_all(&[0u8])?;
            }
        }
    }
    out.flush()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("vidcull-decode-sidecar: {e}");
        std::process::exit(1);
    }
}
