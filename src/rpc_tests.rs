//! Unit tests for `rpc.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod rpc_tests;`.

use super::handle_line;

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
