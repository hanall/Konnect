use konnect_ipc::{builders, gen::kiapi, KiCadIpcClient};
use prost::Message;
use std::sync::{Arc, Mutex};

fn candidate() -> prost_types::Any {
    builders::pack_any(
        &kiapi::board::types::FootprintInstance {
            id: Some(kiapi::common::types::Kiid {
                value: "fp-1".into(),
            }),
            ..Default::default()
        },
        "kiapi.board.types.FootprintInstance",
    )
}

fn exercise(
    source: &str,
    wrong_document: bool,
    items: Vec<prost_types::Any>,
) -> (anyhow::Result<()>, Vec<String>) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let address = format!(
        "inproc://lossless-footprint-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let server = nng::Socket::new(nng::Protocol::Rep0).unwrap();
    server.listen(&address).unwrap();
    let control = server.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let snapshot = source.to_string();
    let document = kiapi::common::types::DocumentSpecifier {
        r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
        identifier: Some(
            kiapi::common::types::document_specifier::Identifier::BoardFilename(
                "/fixture/board.kicad_pcb".into(),
            ),
        ),
        ..Default::default()
    };
    let expected = document.clone();
    let worker = std::thread::spawn(move || {
        while let Ok(request) = server.recv() {
            let request = kiapi::common::ApiRequest::decode(request.as_slice()).unwrap();
            let message = request.message.unwrap();
            let name = message.type_url.rsplit('.').next().unwrap().to_string();
            observed.lock().unwrap().push(name.clone());
            let payload = match name.as_str() {
                "SaveDocumentToString" => {
                    let cmd = kiapi::common::commands::SaveDocumentToString::decode(
                        message.value.as_slice(),
                    )
                    .unwrap();
                    assert_eq!(cmd.document, Some(expected.clone()));
                    builders::pack_any(
                        &kiapi::common::commands::SavedDocumentResponse {
                            document: if wrong_document {
                                None
                            } else {
                                Some(expected.clone())
                            },
                            contents: snapshot.clone(),
                        },
                        "kiapi.common.commands.SavedDocumentResponse",
                    )
                }
                "UpdateItems" => {
                    let cmd =
                        kiapi::common::commands::UpdateItems::decode(message.value.as_slice())
                            .unwrap();
                    builders::pack_any(
                        &kiapi::common::commands::UpdateItemsResponse {
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            updated_items: cmd
                                .items
                                .into_iter()
                                .map(|item| kiapi::common::commands::ItemUpdateResult {
                                    status: Some(kiapi::common::commands::ItemStatus {
                                        code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                                        ..Default::default()
                                    }),
                                    item: Some(item),
                                })
                                .collect(),
                            ..Default::default()
                        },
                        "kiapi.common.commands.UpdateItemsResponse",
                    )
                }
                other => panic!("예상하지 못한 IPC 요청: {other}"),
            };
            let response = kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsOk as i32,
                    ..Default::default()
                }),
                message: Some(payload),
                ..Default::default()
            };
            if server
                .send(nng::Message::from(response.encode_to_vec().as_slice()))
                .is_err()
            {
                break;
            }
        }
    });
    let result = KiCadIpcClient::new(&address).update_items_in(document, items);
    control.close();
    worker.join().unwrap();
    let calls = calls.lock().unwrap().clone();
    (result, calls)
}

#[test]
fn unsaved_native_children_refuse_before_any_update() {
    for child in [
        "(point (at 1.27 0) (size 2) (layer F.Fab))",
        "(group g (members))",
        "(units (unit A))",
        "(component_classes (class control))",
    ] {
        let source = format!("(kicad_pcb (footprint Part (uuid fp-1) {child}))");
        let (result, calls) = exercise(&source, false, vec![candidate()]);
        assert!(
            result.is_err(),
            "지원되지 않는 native 자식을 허용함: {child}"
        );
        assert_eq!(calls, ["SaveDocumentToString"]);
    }
}

#[test]
fn complete_live_snapshot_precedes_an_ordinary_footprint_update() {
    let (result, calls) = exercise(
        "(kicad_pcb (footprint Part (uuid fp-1) (embedded_fonts no)))",
        false,
        vec![candidate()],
    );
    result.unwrap();
    assert_eq!(calls, ["SaveDocumentToString", "UpdateItems"]);
}

#[test]
fn missing_ambiguous_malformed_or_wrong_document_snapshot_is_not_mutated() {
    for (source, wrong) in [
        ("(kicad_pcb)", false),
        (
            "(kicad_pcb (footprint Part (uuid fp-1)) (footprint Part (uuid fp-1)))",
            false,
        ),
        ("(kicad_pcb", false),
        ("(kicad_pcb (footprint Part (uuid fp-1)))", true),
    ] {
        let (result, calls) = exercise(source, wrong, vec![candidate()]);
        assert!(result.is_err());
        assert_eq!(calls, ["SaveDocumentToString"]);
    }
}

#[test]
fn unrelated_item_updates_do_not_take_a_footprint_snapshot() {
    let track = builders::pack_any(
        &kiapi::board::types::Track::default(),
        "kiapi.board.types.Track",
    );
    let (result, calls) = exercise("", false, vec![track]);
    result.unwrap();
    assert_eq!(calls, ["UpdateItems"]);
}

#[test]
fn clipboard_generated_footprint_format_metadata_is_accepted() {
    let source = "(kicad_pcb (footprint Part (version 20260206) (generator pcbnew) (generator_version 10.0) (uuid fp-1)))";
    let (result, calls) = exercise(source, false, vec![candidate()]);
    result.unwrap();
    assert_eq!(calls, ["SaveDocumentToString", "UpdateItems"]);
}

#[test]
fn native_bga_pad_property_is_not_lost_in_a_parent_update() {
    let source = "(kicad_pcb (footprint Part (uuid fp-1) (pad 1 smd circle (at 0 0) (size 1 1) (layers F.Cu) (property pad_prop_bga))))";
    let (result, calls) = exercise(source, false, vec![candidate()]);
    assert!(result.is_err());
    assert_eq!(calls, ["SaveDocumentToString"]);
}
