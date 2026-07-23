use anyhow::{Context, Result};

use crate::provider::{Attachment, ImageDetail};
use crate::store::{BASH_CALL_ID_ENV, SESSION_DB_ENV, SESSION_OWNER_PID_ENV, Store};

pub fn write_image_attachment(attachment: &Attachment, detail: ImageDetail) -> Result<()> {
    let database_path = std::env::var_os(SESSION_DB_ENV)
        .context("view_image is only available inside a live Mu tool call")?;
    let bash_call_id = std::env::var(BASH_CALL_ID_ENV)
        .context("missing Mu Bash-call identity")?
        .parse::<i64>()
        .context("invalid Mu Bash-call identity")?;
    let owner_pid = std::env::var(SESSION_OWNER_PID_ENV)
        .context("missing Mu session owner PID")?
        .parse::<i64>()
        .context("invalid Mu session owner PID")?;

    let store = Store::open(std::path::Path::new(&database_path))?;
    store.append_bash_attachment(bash_call_id, owner_pid, attachment, detail)
}
