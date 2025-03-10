use num_bigint::{BigInt, BigUint};
use num_traits::{Num, Zero};
use std::collections::BTreeMap;

use acvm::FieldElement;
use serde::Serialize;

use crate::errors::InputParserError;
use crate::{Abi, AbiType};

pub mod json;
mod toml;

/// This is what all formats eventually transform into
/// For example, a toml file will parse into TomlTypes
/// and those TomlTypes will be mapped to Value
#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum InputValue {
    Field(FieldElement),
    String(String),
    Vec(Vec<InputValue>),
    Struct(BTreeMap<String, InputValue>),
}

impl InputValue {
    /// Checks whether the ABI type matches the InputValue type
    /// and also their arity
    pub fn matches_abi(&self, abi_param: &AbiType) -> bool {
        match (self, abi_param) {
            (InputValue::Field(_), AbiType::Field) => true,
            (InputValue::Field(field_element), AbiType::Integer { width, .. }) => {
                field_element.num_bits() <= *width
            }
            (InputValue::Field(field_element), AbiType::Boolean) => {
                field_element.is_one() || field_element.is_zero()
            }

            (InputValue::Vec(array_elements), AbiType::Array { length, typ, .. }) => {
                if array_elements.len() != *length as usize {
                    return false;
                }
                // Check that all of the array's elements' values match the ABI as well.
                array_elements.iter().all(|input_value| input_value.matches_abi(typ))
            }

            (InputValue::String(string), AbiType::String { length }) => {
                string.len() == *length as usize
            }

            (InputValue::Struct(map), AbiType::Struct { fields, .. }) => {
                if map.len() != fields.len() {
                    return false;
                }

                let field_types = BTreeMap::from_iter(fields.iter().cloned());

                // Check that all of the struct's fields' values match the ABI as well.
                map.iter().all(|(field_name, field_value)| {
                    if let Some(field_type) = field_types.get(field_name) {
                        field_value.matches_abi(field_type)
                    } else {
                        false
                    }
                })
            }

            (InputValue::Vec(vec_elements), AbiType::Tuple { fields }) => {
                if vec_elements.len() != fields.len() {
                    return false;
                }

                vec_elements
                    .iter()
                    .zip(fields)
                    .all(|(input_value, abi_param)| input_value.matches_abi(abi_param))
            }

            // All other InputValue-AbiType combinations are fundamentally incompatible.
            _ => false,
        }
    }
}

/// The different formats that are supported when parsing
/// the initial witness values
#[cfg_attr(test, derive(strum_macros::EnumIter))]
pub enum Format {
    Json,
    Toml,
}

impl Format {
    pub fn ext(&self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Toml => "toml",
        }
    }
}

impl Format {
    pub fn parse(
        &self,
        input_string: &str,
        abi: &Abi,
    ) -> Result<BTreeMap<String, InputValue>, InputParserError> {
        match self {
            Format::Json => json::parse_json(input_string, abi),
            Format::Toml => toml::parse_toml(input_string, abi),
        }
    }

    pub fn serialize(
        &self,
        input_map: &BTreeMap<String, InputValue>,
        abi: &Abi,
    ) -> Result<String, InputParserError> {
        match self {
            Format::Json => json::serialize_to_json(input_map, abi),
            Format::Toml => toml::serialize_to_toml(input_map, abi),
        }
    }
}

#[cfg(test)]
mod serialization_tests {
    use std::collections::BTreeMap;

    use acvm::FieldElement;
    use strum::IntoEnumIterator;

    use crate::{
        input_parser::InputValue, Abi, AbiParameter, AbiReturnType, AbiType, AbiVisibility, Sign,
        MAIN_RETURN_NAME,
    };

    use super::Format;

    #[test]
    fn serialization_round_trip() {
        let abi = Abi {
            parameters: vec![
                AbiParameter {
                    name: "foo".into(),
                    typ: AbiType::Field,
                    visibility: AbiVisibility::Private,
                },
                AbiParameter {
                    name: "bar".into(),
                    typ: AbiType::Struct {
                        path: "MyStruct".into(),
                        fields: vec![
                            ("field1".into(), AbiType::Integer { sign: Sign::Unsigned, width: 8 }),
                            (
                                "field2".into(),
                                AbiType::Array { length: 2, typ: Box::new(AbiType::Boolean) },
                            ),
                        ],
                    },
                    visibility: AbiVisibility::Private,
                },
            ],
            return_type: Some(AbiReturnType {
                abi_type: AbiType::String { length: 5 },
                visibility: AbiVisibility::Public,
            }),
            // These two fields are unused when serializing/deserializing to file.
            param_witnesses: BTreeMap::new(),
            return_witnesses: Vec::new(),
        };

        let input_map: BTreeMap<String, InputValue> = BTreeMap::from([
            ("foo".into(), InputValue::Field(FieldElement::one())),
            (
                "bar".into(),
                InputValue::Struct(BTreeMap::from([
                    ("field1".into(), InputValue::Field(255u128.into())),
                    (
                        "field2".into(),
                        InputValue::Vec(vec![
                            InputValue::Field(true.into()),
                            InputValue::Field(false.into()),
                        ]),
                    ),
                ])),
            ),
            (MAIN_RETURN_NAME.into(), InputValue::String("hello".to_owned())),
        ]);

        for format in Format::iter() {
            let serialized_inputs = format.serialize(&input_map, &abi).unwrap();

            let reconstructed_input_map = format.parse(&serialized_inputs, &abi).unwrap();

            assert_eq!(input_map, reconstructed_input_map);
        }
    }
}

fn parse_str_to_field(value: &str) -> Result<FieldElement, InputParserError> {
    let big_num = if let Some(hex) = value.strip_prefix("0x") {
        BigUint::from_str_radix(hex, 16)
    } else {
        BigUint::from_str_radix(value, 10)
    };
    big_num.map_err(|err_msg| InputParserError::ParseStr(err_msg.to_string())).and_then(|bigint| {
        if bigint < FieldElement::modulus() {
            Ok(field_from_big_uint(bigint))
        } else {
            Err(InputParserError::ParseStr(format!(
                "Input exceeds field modulus. Values must fall within [0, {})",
                FieldElement::modulus(),
            )))
        }
    })
}

fn parse_str_to_signed(value: &str, witdh: u32) -> Result<FieldElement, InputParserError> {
    let big_num = if let Some(hex) = value.strip_prefix("0x") {
        BigInt::from_str_radix(hex, 16)
    } else {
        BigInt::from_str_radix(value, 10)
    };

    big_num.map_err(|err_msg| InputParserError::ParseStr(err_msg.to_string())).and_then(|bigint| {
        let modulus: BigInt = FieldElement::modulus().into();
        let bigint = if bigint.sign() == num_bigint::Sign::Minus {
            BigInt::from(2).pow(witdh) + bigint
        } else {
            bigint
        };
        if bigint.is_zero() || (bigint.sign() == num_bigint::Sign::Plus && bigint < modulus) {
            Ok(field_from_big_int(bigint))
        } else {
            Err(InputParserError::ParseStr(format!(
                "Input exceeds field modulus. Values must fall within [0, {})",
                FieldElement::modulus(),
            )))
        }
    })
}

fn field_from_big_uint(bigint: BigUint) -> FieldElement {
    FieldElement::from_be_bytes_reduce(&bigint.to_bytes_be())
}

fn field_from_big_int(bigint: BigInt) -> FieldElement {
    match bigint.sign() {
        num_bigint::Sign::Minus => {
            unreachable!(
                "Unsupported negative value; it should only be called with a positive value"
            )
        }
        num_bigint::Sign::NoSign => FieldElement::zero(),
        num_bigint::Sign::Plus => FieldElement::from_be_bytes_reduce(&bigint.to_bytes_be().1),
    }
}

#[cfg(test)]
mod test {
    use acvm::FieldElement;
    use num_bigint::BigUint;

    use super::parse_str_to_field;

    fn big_uint_from_field(field: FieldElement) -> BigUint {
        BigUint::from_bytes_be(&field.to_be_bytes())
    }

    #[test]
    fn parse_empty_str_fails() {
        // Check that this fails appropriately rather than being treated as 0, etc.
        assert!(parse_str_to_field("").is_err());
    }

    #[test]
    fn parse_fields_from_strings() {
        let fields = vec![
            FieldElement::zero(),
            FieldElement::one(),
            FieldElement::from(u128::MAX) + FieldElement::one(),
            // Equivalent to `FieldElement::modulus() - 1`
            -FieldElement::one(),
        ];

        for field in fields {
            let hex_field = format!("0x{}", field.to_hex());
            let field_from_hex = parse_str_to_field(&hex_field).unwrap();
            assert_eq!(field_from_hex, field);

            let dec_field = big_uint_from_field(field).to_string();
            let field_from_dec = parse_str_to_field(&dec_field).unwrap();
            assert_eq!(field_from_dec, field);
        }
    }

    #[test]
    fn rejects_noncanonical_fields() {
        let noncanonical_field = FieldElement::modulus().to_string();
        assert!(parse_str_to_field(&noncanonical_field).is_err());
    }
}
