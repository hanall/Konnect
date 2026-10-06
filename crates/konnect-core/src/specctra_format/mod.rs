// SPDX-FileCopyrightText: 2024 Topola contributors
//
// SPDX-License-Identifier: MIT

//! Konnect-owned minimum Specctra DSN schema, parser, and writer.
//!
//! This module is derived from `topola_specctra` 0.1.0. The format behavior is
//! intentionally retained while geometry convenience APIs unrelated to parsing
//! and serialization are omitted. See `UPSTREAM.md` for provenance.

pub mod error;
pub mod read;
pub mod structure;
pub mod write;

use error::{ParseError, ParseErrorContext};
use read::{ListTokenizer, ReadDsn};
use std::io::BufRead;
use structure::DsnFile;

pub enum ListToken {
    Start { name: String },
    Leaf { value: String },
    End,
}

impl ListToken {
    pub fn is_start_of(&self, valid_names: &[&'static str]) -> bool {
        if let Self::Start { name: actual_name } = self {
            valid_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(actual_name))
        } else {
            false
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Start { name } => 1 + name.len(),
            Self::Leaf { value } => value.len(),
            Self::End => 1,
        }
    }
}

/// Parse exactly one complete DSN document.
///
/// The upstream parser validates the schema through the root closing token.
/// Konnect additionally rejects any non-whitespace trailing token so a valid
/// prefix cannot conceal unvalidated input.
pub fn read_complete_dsn<R: BufRead>(reader: R) -> Result<DsnFile, ParseErrorContext> {
    let mut tokenizer = ListTokenizer::new(reader);
    let document = DsnFile::read_dsn(&mut tokenizer)?;
    match tokenizer.consume_token() {
        Err(error) if matches!(error.error, ParseError::Eof) => Ok(document),
        Err(error) => Err(error),
        Ok(token) => Err(ParseError::Expected("end of file").add_context(token.context)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn parse(source: &str) -> Result<DsnFile, ParseErrorContext> {
        read_complete_dsn(BufReader::new(Cursor::new(source.as_bytes())))
    }

    #[test]
    fn official_native_and_freerouting_corpus_parse_completely() {
        let native =
            include_str!("../../tests/fixtures/specctra_two_resistors.native-kicad-10.dsn");
        let freerouting =
            include_str!("../../tests/fixtures/freerouting_issue269_minimal_v2_3_0.dsn");

        parse(native).expect("KiCad 10 native DSN fixture must remain parseable");
        parse(freerouting).expect("Freerouting v2.3.0 DSN fixture must remain parseable");
    }

    #[test]
    fn malformed_dsn_mutations_are_rejected() {
        let valid = include_str!("../../tests/fixtures/freerouting_issue269_minimal_v2_3_0.dsn");
        parse(valid).expect("control fixture must parse before mutation");

        let truncated = valid
            .strip_suffix(")\n")
            .expect("fixture has final root close")
            .to_string();
        let invalid_number = valid.replacen("(resolution um 10)", "(resolution um invalid)", 1);
        let missing_required_wiring = valid.replacen("(wiring)", "(unknown_wiring)", 1);
        let unterminated_quote =
            valid.replacen("(host_version \"8.0.1\")", "(host_version \"8.0.1)", 1);
        let trailing_unvalidated_form = format!("{valid}(concealed_payload)\n");

        for (label, mutation) in [
            ("truncated root", truncated),
            ("invalid number", invalid_number),
            ("missing required wiring", missing_required_wiring),
            ("unterminated quote", unterminated_quote),
            ("trailing form", trailing_unvalidated_form),
        ] {
            assert!(
                parse(&mutation).is_err(),
                "mutation parsed unexpectedly: {label}"
            );
        }
    }
}
