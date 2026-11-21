//! The first-boot credential document: the one file that holds an image's generated
//! password.
//!
//! Every image a build or a press assembles gets a password of its own. It is drawn when
//! the image is assembled, and spliced into its `/etc/shadow` as a hash. The plaintext has
//! to reach the operator, and it reaches them through this document and nowhere else. It
//! is written beside the image as `<image stem>.credentials.toml`, created readable by its
//! owner only, and named in the build's output by path. It is never in the provenance
//! manifest, the bills of materials, the artifact inventory, or the event stream. Those
//! are records made to be shared, uploaded, and kept.
//!
//! Expiry is not a substitute for keeping it private. The image ships the password
//! expired, which forces whoever logs in first to change it, and that is not
//! necessarily the owner.
//!
//! Pure: the document type and its TOML form. The engine creates the file privately and
//! the CLI names it.

use crate::error::ConfigError;
use serde::{Deserialize, Serialize};

/// Banner prepended to a serialized credential document.
const BANNER: &str = "\
# boot2deb first-boot credential. SECRET: this file is the only copy of the image's
# generated password. It is created readable by its owner only; keep it that way, and
# do not publish it with the image.
";

/// The file name of an image's credential document: `<stem>.credentials.toml`, where
/// `stem` is the build point's artifact stem or a pressed image's file stem.
pub fn credential_name(stem: &str) -> String {
    format!("{stem}.credentials.toml")
}

/// An image's generated first-boot password and what it opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstBootCredential {
    /// The file name of the image the password opens, beside this document. A build's
    /// compressed containers hold the same disk, so the build names its raw image.
    pub image: String,
    /// The account the password is for: the image's default account.
    pub user: String,
    /// The password, in plaintext.
    pub password: String,
    /// How the password behaves on the image.
    pub note: String,
}

/// How every generated password behaves on the image it was drawn for.
pub const NOTE: &str = "expired at first login (passwd -e); unique per assembled image";

impl FirstBootCredential {
    /// The credential for `user` on the image `image`.
    pub fn new(image: &str, user: &str, password: &str) -> Self {
        Self {
            image: image.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            note: NOTE.to_string(),
        }
    }

    /// The document as written: the banner, then the TOML body.
    pub fn to_toml(&self) -> String {
        let body = toml::to_string(self).expect("a credential always serializes");
        format!("{BANNER}{body}")
    }

    /// Parse a credential document. `origin` names the file in an error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`] for a document that is not a credential.
    pub fn from_toml_str(text: &str, origin: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: origin.to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_round_trips_and_says_it_is_secret() {
        let c = FirstBootCredential::new("board-forky.img", "debian", "Kp7rTx");
        let text = c.to_toml();
        assert!(text.starts_with("# boot2deb first-boot credential. SECRET"));
        assert_eq!(
            FirstBootCredential::from_toml_str(&text, "c.toml").unwrap(),
            c
        );
        assert!(FirstBootCredential::from_toml_str("user = \"x\"\n", "c.toml").is_err());
        assert_eq!(
            credential_name("board-forky"),
            "board-forky.credentials.toml"
        );
    }
}
