//! The Remove question (SPEC §8.3) as a view model: what "Remove…" and "Remove and Delete Files…" ask before anything
//! goes. Rust words it (docs/CROSS-PLATFORM.md §1); the host draws it (under the row for one transfer, in the title bar
//! for several), picks its icon from [`RemovalQuestion::delete_files`] and calls `remove(ids, delete_files)` on the
//! confirm button.

use trs_platform::{return_key, trash};

use crate::menu::APP_NAME;
use crate::types::TransferRow;

/// The question, with every text it shows.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RemovalQuestion {
    /// The transfers it is about.
    pub ids: Vec<String>,
    /// More than one transfer.
    pub plural: bool,
    /// The files go to the Trash too (the host's "delete" icon); otherwise they stay.
    pub delete_files: bool,
    /// "Remove “Sintel”? Files are kept."
    pub message: String,
    /// "Cancel".
    pub cancel_title: String,
    /// "Keep it".
    pub cancel_help: String,
    /// "Remove", or "Delete" when the files go too: drawn as a destructive button.
    pub confirm_title: String,
    pub confirm_help: String,
}

/// The question for removing `rows`, with (`delete_files`) or without their files.
#[uniffi::export]
pub fn removal_question(rows: Vec<TransferRow>, delete_files: bool) -> RemovalQuestion {
    let plural = rows.len() != 1;
    let what = match rows.as_slice() {
        [one] => format!("“{}”", one.display_name),
        _ => format!("{} transfers", rows.len()),
    };
    let message = if delete_files {
        format!(concat!("Remove {} and move {} files to the ", trash!(), "?"), what, if plural { "their" } else { "its" })
    } else {
        format!("Remove {what}? Files are kept.")
    };
    let (confirm_title, confirm_help) = if delete_files {
        ("Delete", format!(concat!("Remove from {} and move the downloaded files to the ", trash!(), " (", return_key!(), ")"), APP_NAME))
    } else {
        ("Remove", format!(concat!("Remove from {}; the downloaded files stay where they are (", return_key!(), ")"), APP_NAME))
    };
    RemovalQuestion {
        ids: rows.into_iter().map(|r| r.id).collect(),
        plural,
        delete_files,
        message,
        cancel_title: "Cancel".into(),
        cancel_help: if plural { "Keep them" } else { "Keep it" }.into(),
        confirm_title: confirm_title.into(),
        confirm_help,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_text::tests::row;
    use crate::types::TransferState;

    fn named(id: &str, name: &str) -> TransferRow {
        TransferRow { display_name: name.into(), ..row(id, TransferState::Downloading) }
    }

    #[test]
    fn one_transfer_is_named_and_its_files_are_kept_unless_asked() {
        let q = removal_question(vec![named("a", "Sintel")], false);
        assert_eq!((q.ids.as_slice(), q.plural, q.delete_files), (&["a".to_string()][..], false, false));
        assert_eq!(q.message, "Remove “Sintel”? Files are kept.");
        assert_eq!((q.cancel_title.as_str(), q.cancel_help.as_str()), ("Cancel", "Keep it"));
        assert_eq!(q.confirm_title, "Remove");
        assert_eq!(q.confirm_help, concat!("Remove from Seedwell; the downloaded files stay where they are (", return_key!(), ")"));

        let q = removal_question(vec![named("a", "Sintel")], true);
        assert!(q.delete_files && !q.plural);
        assert_eq!(q.message, concat!("Remove “Sintel” and move its files to the ", trash!(), "?"));
        assert_eq!(q.confirm_title, "Delete");
        assert_eq!(q.confirm_help, concat!("Remove from Seedwell and move the downloaded files to the ", trash!(), " (", return_key!(), ")"));
    }

    #[test]
    fn several_transfers_are_counted() {
        let rows = vec![named("a", "Sintel"), named("b", "Tears of Steel"), named("c", "Big Buck Bunny")];
        let q = removal_question(rows.clone(), false);
        assert_eq!(q.ids, ["a", "b", "c"]);
        assert!(q.plural);
        assert_eq!(q.message, "Remove 3 transfers? Files are kept.");
        assert_eq!(q.cancel_help, "Keep them");
        let q = removal_question(rows, true);
        assert_eq!(q.message, concat!("Remove 3 transfers and move their files to the ", trash!(), "?"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mac_words_are_the_ones_it_always_had() {
        let q = removal_question(vec![named("a", "Sintel")], true);
        assert_eq!(q.message, "Remove “Sintel” and move its files to the Trash?");
        assert_eq!(q.confirm_help, "Remove from Seedwell and move the downloaded files to the Trash (Return)");
        let q = removal_question(vec![named("a", "Sintel"), named("b", "B")], false);
        assert_eq!(q.confirm_help, "Remove from Seedwell; the downloaded files stay where they are (Return)");
    }
}
