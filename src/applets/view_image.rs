use std::path::PathBuf;

use clap::Parser;

use crate::artifact::write_image_artifact;
use crate::attachment::load_attachment;
use crate::provider::ImageDetail;

#[derive(Debug, Parser)]
#[command(
    name = "view_image",
    about = "Load an image into the current Mu tool result"
)]
struct Args {
    #[arg(long, value_enum, default_value_t = ImageDetail::Auto)]
    detail: ImageDetail,
    path: PathBuf,
}

pub fn main() -> i32 {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    match run(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("view_image: {error:#}");
            1
        }
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let attachment = load_attachment(&args.path)?;
    if !attachment.media_type.starts_with("image/") {
        anyhow::bail!("unsupported image type: {}", attachment.media_type);
    }
    write_image_artifact(&attachment, args.detail)?;
    println!(
        "Viewed image: {} ({}, {} bytes, detail={})",
        attachment.filename,
        attachment.media_type,
        attachment.data.len(),
        args.detail
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_is_optional_and_defaults_to_auto() {
        let args = Args::try_parse_from(["view_image", "image.png"]).unwrap();
        assert_eq!(args.detail, ImageDetail::Auto);
        assert_eq!(args.path, PathBuf::from("image.png"));
    }

    #[test]
    fn accepts_every_detail_value() {
        for (value, expected) in [
            ("auto", ImageDetail::Auto),
            ("low", ImageDetail::Low),
            ("high", ImageDetail::High),
            ("original", ImageDetail::Original),
        ] {
            let args =
                Args::try_parse_from(["view_image", "--detail", value, "image.png"]).unwrap();
            assert_eq!(args.detail, expected);
        }
    }
}
