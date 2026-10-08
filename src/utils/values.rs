use serde_json::Value;

/// Preserve the shortest float4 decimal before storing it in JSON's f64 number.
pub(crate) fn float4_to_json(value: f32) -> Value {
    Value::from(value.to_string().parse::<f64>().ok())
}
