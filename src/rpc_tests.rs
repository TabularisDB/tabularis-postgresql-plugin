//! Unit tests for `rpc.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod rpc_tests;`.

use super::{handle_line, peek_method};

/// A non-object JSON-RPC line (array) must produce a `-32600 Invalid Request`
/// error response, not be silently swallowed as a notification (#135).
#[tokio::test]
async fn non_object_array_returns_invalid_request_error() {
    let response = handle_line("[1,2,3]").await;
    let response = response.expect("a non-object request must get a response, not None");
    assert_eq!(response.get("id").and_then(|v| v.as_null()), Some(()));
    let err = response.get("error").expect("must have an error");
    assert_eq!(
        err.get("code").and_then(|c| c.as_i64()),
        Some(-32600),
        "non-object input is an Invalid Request, not a Method Not Found (-32601)"
    );
}

/// A bare number is non-object and must get -32600.
#[tokio::test]
async fn non_object_number_returns_invalid_request_error() {
    let response = handle_line("42").await;
    let response = response.expect("a non-object request must get a response");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32600)
    );
}

/// A bare string is non-object and must get -32600.
#[tokio::test]
async fn non_object_string_returns_invalid_request_error() {
    let response = handle_line("\"hello\"").await;
    let response = response.expect("a non-object request must get a response");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32600)
    );
}

/// A bare boolean is non-object and must get -32600.
#[tokio::test]
async fn non_object_boolean_returns_invalid_request_error() {
    let response = handle_line("true").await;
    let response = response.expect("a non-object request must get a response");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32600)
    );
}

/// `null` is non-object and must get -32600 (serde_json parses bare `null`
/// to `Value::Null`, which is not an object).
#[tokio::test]
async fn non_object_null_returns_invalid_request_error() {
    let response = handle_line("null").await;
    let response = response.expect("a non-object request must get a response");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32600)
    );
}

/// A valid request with an `id` must get a response (not None) — guards
/// against the fix accidentally over-suppressing legitimate requests or the
/// guard firing for objects. Asserting the error code is NOT -32600 (the
/// guard's code for non-objects) distinguishes "guard misfired on an object"
/// from a normal handler error (e.g. -32603 connection error when no DB is
/// available, which is expected here — `ping` tries to connect).
#[tokio::test]
async fn valid_request_with_id_returns_a_response() {
    let response = handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#).await;
    let response = response.expect("a request with an id must get a response, not None");
    let code = response
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_i64());
    assert!(
        code != Some(-32600),
        "the non-object guard must not fire for a valid JSON object; got -32600"
    );
}

/// A notification (object with no `id` field) must return None — the fix
/// must not break the cancel-notification path (#126). Uses a high request
/// id (999_999) that won't collide with `cancel_tests.rs`'s ids (1, 2) —
/// `cancel` mutates the global `HANDLES` map, so sharing an id with another
/// parallel test is a latent flaky-test hazard.
#[tokio::test]
async fn notification_without_id_returns_none() {
    let response =
        handle_line(r#"{"jsonrpc":"2.0","method":"cancel","params":{"id":999999}}"#).await;
    assert!(
        response.is_none(),
        "a notification (object with no id) must return None so main.rs skips the stdout write"
    );
}

/// A parse error must return -32700 (pre-existing behavior, must not regress).
#[tokio::test]
async fn parse_error_returns_32700() {
    let response = handle_line("not valid json").await;
    let response = response.expect("a parse error must get a response");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32700)
    );
}

/// An object with an `id` but no `method` is an Invalid Request (-32600),
/// not a Method Not Found (-32601) — per JSON-RPC 2.0, a Request object MUST
/// contain a `method` member. The old code fell through to the `other` arm
/// with an empty method string, producing a misleading -32601 (#137).
/// Also asserts the response echoes the request's `id` (not `null`) — a
/// regression to `Value::Null` would make the host fail to deserialize the
/// response (it expects `id: u64`), causing the caller to hang.
#[tokio::test]
async fn object_with_id_but_no_method_returns_invalid_request() {
    let response = handle_line(r#"{"jsonrpc":"2.0","id":1}"#).await;
    let response = response.expect("an object with an id must get a response");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32600),
        "an object with an id but no method is an Invalid Request, not a Method Not Found"
    );
    assert_eq!(
        response.get("id").and_then(|v| v.as_u64()),
        Some(1),
        "the error response must echo the request's id, not null — the host \
         deserializes id as u64 and a null id would cause a deserialization failure"
    );
}

/// An empty object `{}` (no `id`, no `method`) is an Invalid Request, not a
/// notification — per JSON-RPC 2.0, a notification must still be a valid
/// Request (have a `method`). The old code treated it as a notification (no
/// `id` → `is_notification = true`) and silently swallowed it → the client
/// hung (#137, same bug class as #135 but for objects).
#[tokio::test]
async fn empty_object_returns_invalid_request_not_swallowed() {
    let response = handle_line("{}").await;
    let response =
        response.expect("an empty object must get a response, not be swallowed as a notification");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32600),
        "an empty object is an Invalid Request, not a notification"
    );
}

/// An object with no `id` and no `method` (just `jsonrpc` and `params`) is
/// also an Invalid Request — it's not a notification because it has no
/// `method`, and it's not a valid request either.
#[tokio::test]
async fn object_with_no_id_and_no_method_returns_invalid_request() {
    let response = handle_line(r#"{"jsonrpc":"2.0","params":{"foo":"bar"}}"#).await;
    let response = response.expect("an object with no method must get a response");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32600),
        "an object with no method is an Invalid Request, not a notification"
    );
}

/// An object with a non-string `method` (e.g. a number) is an Invalid Request
/// — the `method` member must be a string per JSON-RPC 2.0. The old code
/// coerced it to `""` via `unwrap_or("")` and fell through to -32601.
#[tokio::test]
async fn object_with_non_string_method_returns_invalid_request() {
    let response = handle_line(r#"{"jsonrpc":"2.0","id":1,"method":42}"#).await;
    let response = response.expect("an object with a non-string method must get a response");
    assert_eq!(
        response
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32600),
        "a non-string method is an Invalid Request, not a Method Not Found"
    );
}

// --- peek_method (#167 routing hint) ----------------------------------------
//
// `peek_method` is the routing hint `run_reader` uses to send a line to the
// cancel lane, lifecycle lane, or general queue before a worker pulls it — so
// a `cancel` notification is never stuck behind the 4 general workers it's
// meant to relieve. It returns the method name (Some) for a valid object
// with a string `method`, None otherwise (parse error / non-object /
// missing-or-non-string method → general queue, where `handle_line` surfaces
// the proper error). These tests pin the classification `run_reader` relies on.

#[test]
fn peek_method_reads_known_methods() {
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","method":"ping","id":1,"params":{}}"#).as_deref(),
        Some("ping")
    );
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","method":"test_connection","id":2}"#).as_deref(),
        Some("test_connection")
    );
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","method":"initialize","id":3}"#).as_deref(),
        Some("initialize")
    );
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","method":"shutdown","id":4}"#).as_deref(),
        Some("shutdown")
    );
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","method":"cancel","params":{"id":5}}"#).as_deref(),
        Some("cancel")
    );
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","method":"execute_query","id":6,"params":{}}"#).as_deref(),
        Some("execute_query")
    );
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","method":"get_tables","id":7}"#).as_deref(),
        Some("get_tables")
    );
}

#[test]
fn peek_method_reads_a_cancel_notification_with_no_id() {
    // A fire-and-forget cancel notification (the form the host sends on its
    // call timeout, #126) has no top-level `id`. `peek_method` must still see
    // `"cancel"` so `run_reader` routes it to the cancel lane — this is the
    // relief-valve path #167 depends on.
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","method":"cancel","params":{"id":42}}"#).as_deref(),
        Some("cancel")
    );
}

#[test]
fn peek_method_returns_none_for_non_object() {
    // Non-objects route to the general queue, where `handle_line` surfaces
    // -32600 Invalid Request (#135). The peek must not panic or misclassify.
    assert_eq!(peek_method("[1,2,3]"), None);
    assert_eq!(peek_method("42"), None);
    assert_eq!(peek_method("\"hello\""), None);
    assert_eq!(peek_method("null"), None);
    assert_eq!(peek_method("true"), None);
}

#[test]
fn peek_method_returns_none_for_parse_error() {
    // A malformed line routes to the general queue, where `handle_line`
    // surfaces -32700 parse error.
    assert_eq!(peek_method("not json"), None);
    assert_eq!(peek_method(""), None);
    assert_eq!(peek_method("{"), None);
}

#[test]
fn peek_method_returns_none_for_missing_or_non_string_method() {
    // An object without a string `method` routes to the general queue, where
    // `handle_line` surfaces -32600 (#137). The peek must not treat a
    // missing/non-string method as a valid method name.
    assert_eq!(peek_method(r#"{"jsonrpc":"2.0","id":1}"#), None);
    assert_eq!(peek_method(r#"{"jsonrpc":"2.0","id":1,"method":42}"#), None);
    assert_eq!(
        peek_method(r#"{"jsonrpc":"2.0","id":1,"method":null}"#),
        None
    );
}

#[test]
fn peek_method_ignores_params_and_other_fields() {
    // Only `method` matters for routing; params/id/extra fields don't affect it.
    assert_eq!(
        peek_method(
            r#"{"jsonrpc":"2.0","method":"ping","params":{"host":"db","port":5432},"id":99}"#
        )
        .as_deref(),
        Some("ping")
    );
    assert_eq!(
        peek_method(r#"{"method":"execute_query","extra":"ignored","id":1}"#).as_deref(),
        Some("execute_query")
    );
}
