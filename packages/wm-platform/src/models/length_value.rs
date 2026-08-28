use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

/// Maximum absolute pixel length accepted from config or commands.
pub const MAX_LENGTH_PIXELS: f32 = 100_000.0;

/// Maximum absolute normalized percentage accepted from config or
/// commands. `10.0` corresponds to `1000%`.
pub const MAX_LENGTH_PERCENTAGE: f32 = 10.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LengthValue {
  pub amount: f32,
  pub unit: LengthUnit,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LengthUnit {
  Percentage,
  Pixel,
}

impl LengthValue {
  /// Constructs a validated external length value.
  pub fn try_new(
    amount: f32,
    unit: LengthUnit,
  ) -> Result<Self, crate::ParseError> {
    let max = match unit {
      LengthUnit::Pixel => MAX_LENGTH_PIXELS,
      LengthUnit::Percentage => MAX_LENGTH_PERCENTAGE,
    };

    if amount.is_finite() && amount.abs() <= max {
      Ok(Self { amount, unit })
    } else {
      Err(crate::ParseError::Length(amount.to_string()))
    }
  }

  #[must_use]
  pub fn from_px(px: i32) -> Self {
    Self {
      #[allow(clippy::cast_precision_loss)]
      amount: px as f32,
      unit: LengthUnit::Pixel,
    }
  }

  #[must_use]
  pub fn to_px(&self, total_px: i32, scale_factor: Option<f32>) -> i32 {
    let scale_factor = scale_factor.unwrap_or(1.0);

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    match self.unit {
      LengthUnit::Percentage => (self.amount * total_px as f32) as i32,
      LengthUnit::Pixel => (self.amount * scale_factor) as i32,
    }
  }

  #[must_use]
  pub fn to_percentage(&self, total_px: i32) -> f32 {
    match self.unit {
      LengthUnit::Percentage => self.amount,
      #[allow(clippy::cast_precision_loss)]
      LengthUnit::Pixel => self.amount / total_px as f32,
    }
  }
}

impl FromStr for LengthValue {
  type Err = crate::ParseError;

  /// Parses a string containing a number followed by a unit (`px`, `%`).
  /// Allows for negative numbers.
  ///
  /// Example:
  /// ```
  /// # use wm_platform::{LengthValue, LengthUnit};
  /// # use std::str::FromStr;
  /// let check = LengthValue {
  ///   amount: 100.0,
  ///   unit: LengthUnit::Pixel,
  /// };
  /// let parsed = LengthValue::from_str("100px");
  /// assert_eq!(parsed.unwrap(), check);
  /// ```
  fn from_str(unparsed: &str) -> Result<Self, crate::ParseError> {
    let unparsed = unparsed.trim();
    let (numeric, unit) = if let Some(value) = unparsed.strip_suffix("px")
    {
      (value, LengthUnit::Pixel)
    } else if let Some(value) = unparsed.strip_suffix('%') {
      (value, LengthUnit::Percentage)
    } else {
      (unparsed, LengthUnit::Pixel)
    };

    if !valid_decimal_syntax(numeric) {
      return Err(crate::ParseError::Length(unparsed.to_string()));
    }

    let parsed = numeric
      .parse::<f32>()
      .map_err(|_| crate::ParseError::Length(unparsed.to_string()))?;
    let amount = if unit == LengthUnit::Percentage {
      parsed / 100.0
    } else {
      parsed
    };

    Self::try_new(amount, unit)
      .map_err(|_| crate::ParseError::Length(unparsed.to_string()))
  }
}

/// Accept one optional leading sign and one decimal point, with at least
/// one digit. Exponent notation and internal whitespace are intentionally
/// rejected.
fn valid_decimal_syntax(value: &str) -> bool {
  let unsigned = value
    .strip_prefix('+')
    .or_else(|| value.strip_prefix('-'))
    .unwrap_or(value);
  if unsigned.is_empty() {
    return false;
  }

  let mut decimal_points = 0;
  let mut digits = 0;
  for character in unsigned.chars() {
    if character.is_ascii_digit() {
      digits += 1;
    } else if character == '.' {
      decimal_points += 1;
      if decimal_points > 1 {
        return false;
      }
    } else {
      return false;
    }
  }

  digits > 0
}

/// Deserialize a `LengthValue` from either a string or a struct.
impl<'de> Deserialize<'de> for LengthValue {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StructuredLengthValue {
      amount: f32,
      unit: LengthUnit,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum LengthValueDe {
      Struct(StructuredLengthValue),
      String(String),
    }

    match LengthValueDe::deserialize(deserializer)? {
      LengthValueDe::Struct(value) => {
        Self::try_new(value.amount, value.unit)
          .map_err(serde::de::Error::custom)
      }
      LengthValueDe::String(str) => {
        Self::from_str(&str).map_err(serde::de::Error::custom)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn whole_input_length_parser_accepts_defined_syntax() {
    let cases = [
      ("100px", 100.0, LengthUnit::Pixel),
      ("100", 100.0, LengthUnit::Pixel),
      ("+1.5px", 1.5, LengthUnit::Pixel),
      ("-.5px", -0.5, LengthUnit::Pixel),
      ("1.px", 1.0, LengthUnit::Pixel),
      ("10%", 0.1, LengthUnit::Percentage),
      ("-2%", -0.02, LengthUnit::Percentage),
      (" +2.5% ", 0.025, LengthUnit::Percentage),
    ];

    for (input, amount, unit) in cases {
      let parsed = LengthValue::from_str(input)
        .unwrap_or_else(|error| panic!("rejected {input}: {error}"));
      assert_eq!(parsed.amount, amount, "wrong amount for {input}");
      assert_eq!(parsed.unit, unit, "wrong unit for {input}");
    }
  }

  #[test]
  fn whole_input_length_parser_rejects_junk_and_non_finite_values() {
    for input in [
      "",
      "+",
      "-",
      ".",
      "junk100pxmore",
      "100pxmore",
      "junk100px",
      "100 px",
      "100PX",
      "1.2.3px",
      "1e3px",
      "NaNpx",
      "infpx",
      "-inf%",
      "10%%",
    ] {
      assert!(LengthValue::from_str(input).is_err(), "accepted {input}");
    }
  }

  #[test]
  fn length_practical_boundaries_are_enforced() {
    for input in ["100000px", "-100000px", "1000%", "-1000%"] {
      assert!(LengthValue::from_str(input).is_ok(), "rejected {input}");
    }
    for input in ["100000.1px", "-100000.1px", "1000.1%", "-1000.1%"] {
      assert!(LengthValue::from_str(input).is_err(), "accepted {input}");
    }
    for amount in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
      assert!(LengthValue::try_new(amount, LengthUnit::Pixel).is_err());
      assert!(
        LengthValue::try_new(amount, LengthUnit::Percentage).is_err()
      );
    }
  }
}
