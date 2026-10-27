//! Run the GCS emulator (see `pail::gcs_emulator`) until killed.
//! `pail-gcs-emulator [addr]` (default `127.0.0.1:4443`), then
//! `PAIL_GCS_BASE_URL=http://<addr> pail ... gs://bucket/prefix`.

fn main() -> anyhow::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:4443".into());
    let emu = pail::gcs_emulator::GcsEmulator::start(&addr)?;
    eprintln!("pail GCS emulator at {}", emu.url());
    loop {
        std::thread::park();
    }
}
