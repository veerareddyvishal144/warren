//! `warren screenshot` and `warren input`: the screen of a mac instance.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;
use warren_cli::cli::{InputArgs, ScreenshotArgs};
use warren_cli::{EXIT_OK, Failure};

use super::Ctx;

/// `warren screenshot <id> [-o PATH]`: write the PNG to a file (or stdout
/// with `-o -`) and print where it went with its size.
pub async fn screenshot(ctx: &Ctx, args: ScreenshotArgs) -> Result<i32, Failure> {
    let api = ctx.api()?;
    let shot = api.screenshot(&args.id).await?;
    if args.output.as_deref() == Some(Path::new("-")) {
        let mut out = std::io::stdout().lock();
        out.write_all(&shot.png)?;
        out.flush()?;
        return Ok(EXIT_OK);
    }
    let path = args.output.unwrap_or_else(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        PathBuf::from(format!("screenshot-{}-{now}.png", args.id))
    });
    std::fs::write(&path, &shot.png)?;
    let v = json!({
        "path": path.to_string_lossy(),
        "width": shot.width,
        "height": shot.height,
        "bytes": shot.png.len(),
    });
    ctx.emit(&v, || {
        format!(
            "{} ({}x{}, {} bytes)\n",
            path.display(),
            shot.width,
            shot.height,
            shot.png.len()
        )
    })
}

/// `warren input <id> click|move|type|key|scroll ...`: one action, sent as
/// is; the server validates and answers 204.
pub async fn input(ctx: &Ctx, args: InputArgs) -> Result<i32, Failure> {
    let api = ctx.api()?;
    let action = args.action.action();
    api.input(&args.id, std::slice::from_ref(&action)).await?;
    let v = json!({ "id": args.id, "actions": [action] });
    ctx.emit(&v, || "ok\n".to_string())
}
