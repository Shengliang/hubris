use anyhow::Result;
use std::env;
use std::fs::File;
use std::path::Path;

use build_i2c::{I2cConfigDisposition, I2cConfigGenerator};

fn codegen() -> Result<()> {
    use std::io::Write;

    let out_dir = env::var("OUT_DIR")?;
    let dest_path = Path::new(&out_dir).join("i2c_config.rs");
    let mut file = File::create(&dest_path)?;

    cfg_if::cfg_if! {
        if #[cfg(feature = "standalone")] {
            let disposition = I2cConfigDisposition::Standalone;
            let mut g = I2cConfigGenerator::new(disposition);
        } else {
            let disposition = I2cConfigDisposition::NoController;
            let mut g = I2cConfigGenerator::new(disposition);
        }
    }

    g.generate_header()?;
    g.generate_devices()?;
    g.generate_footer()?;

    file.write_all(g.output.as_bytes())?;

    Ok(())
}

fn main() {
    build_util::expose_target_board();

    if let Err(e) = codegen() {
        println!("code generation failed: {}", e);
        std::process::exit(1);
    }

    println!("cargo:rerun-if-changed=build.rs");
}
