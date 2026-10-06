# Upstream provenance

This parser, writer, error model, and format schema are derived from the
official crates.io release `topola_specctra` version `0.1.0`.

- Package: `https://crates.io/crates/topola_specctra/0.1.0`
- Repository: `https://codeberg.org/topola/topola`
- Path in repository: `specctra`
- VCS commit recorded by the crate: `9825a77dd48148513808494bcd44b7e06ee254d7`
- crates.io SHA-256 checksum:
  `55f4109539b73c1cbed42da864d9aa6b092c832c4583866f8ae05c14a0c1d8c7`
- License: MIT
- Copyright: 2024 Topola contributors

The following upstream files were copied from that release and retain their
SPDX headers:

- `src/error.rs`
- `src/read.rs`
- `src/structure.rs`
- `src/write.rs`
- the `ListToken` definition and implementation from `src/lib.rs`

Konnect deliberately omits geometry convenience APIs that are unrelated to
DSN/SES parsing and serialization:

- `Boundary::to_poly`
- `Polygon::to_poly`
- conversions between schema `Point` and `geo_types::Point`
- `Place::point_with_rotation`
- `Pin::point_with_rotation`

The minimum module also omits Topola's unused SES model (`SesFile`, `Session`,
`Routes`, `NetworkOut`, `NetOut`, and `WireOut`) and its `Dummy` derive fixture.
Konnect does not call that model: `specctra_ses.rs` owns an independent strict
SES import planner. The active `DsnFile` schema is retained. Unused convenience
methods such as `Library::find_padstack_by_name` are likewise outside the
parser/writer contract.

The DSN tokenizer, parser, schema fields, aliases, writer, and error behavior
are otherwise retained. Konnect owns regression tests that pin the prior
writer's golden bytes, official KiCad native DSN parsing, Freerouting corpus
parsing, and malformed-input refusals. Konnect additionally rejects trailing
tokens after a complete DSN root so an otherwise valid prefix cannot conceal
unvalidated input.
