use super::values::float4_to_json;

#[test]
fn finite_float4_extremes_round_trip_without_losing_precision() {
    for value in [
        f32::MAX,
        f32::MIN,
        f32::MIN_POSITIVE,
        f32::from_bits(1),
        f32::from_bits(0x15ae43fd),
        -f32::from_bits(0x15ae43fd),
        -0.0,
    ] {
        let json = float4_to_json(value);
        let decoded = json.as_f64().expect("finite float4 is a JSON number") as f32;
        assert_eq!(decoded.to_bits(), value.to_bits(), "input: {value}");
    }
}
