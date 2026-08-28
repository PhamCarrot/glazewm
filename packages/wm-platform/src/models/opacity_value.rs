use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpacityValue(pub f32);

impl OpacityValue {
  /// Constructs an opacity from a normalized finite value.
  ///
  /// This is the validation boundary shared by command parsing and config
  /// deserialization. Invalid values must never reach a native alpha
  /// update, where they could otherwise make a real window unexpectedly
  /// transparent.
  pub fn try_new(value: f32) -> Result<Self, crate::ParseError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
      Ok(Self(value))
    } else {
      Err(crate::ParseError::Opacity(value.to_string()))
    }
  }

  #[must_use]
  pub fn to_alpha(&self) -> u8 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let alpha = (self.0 * 255.0).round() as u8;
    alpha
  }

  #[must_use]
  pub fn from_alpha(alpha: u8) -> Self {
    Self(f32::from(alpha) / 255.0)
  }

  /// Interpolates between this opacity value and another opacity value.
  /// `progress` should be a value between 0.0 (this opacity) and 1.0
  /// (other opacity).
  #[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
  )]
  #[must_use]
  pub fn interpolate(&self, other: &OpacityValue, progress: f32) -> Self {
    let start_alpha = self.to_alpha() as f32;
    let end_alpha = other.to_alpha() as f32;
    let alpha = start_alpha + (end_alpha - start_alpha) * progress;

    OpacityValue::from_alpha((alpha.round() as u32).clamp(0, 255) as u8)
  }
}

impl Default for OpacityValue {
  fn default() -> Self {
    Self(1.0)
  }
}

impl FromStr for OpacityValue {
  type Err = crate::ParseError;

  /// Parses a string for an opacity value. The string must be a percentage
  /// or a decimal number.
  ///
  /// Example:
  /// ```
  /// # use wm_platform::{OpacityValue};
  /// # use std::str::FromStr;
  /// let check = OpacityValue(0.75);
  /// let parsed = OpacityValue::from_str("75%");
  /// assert_eq!(parsed.unwrap(), check);
  /// ```
  fn from_str(unparsed: &str) -> Result<Self, crate::ParseError> {
    let unparsed = unparsed.trim();

    if let Some(raw_percentage) = unparsed.strip_suffix('%') {
      let percentage = raw_percentage
        .parse::<f32>()
        .map_err(|_| crate::ParseError::Opacity(unparsed.to_string()))?;

      Self::try_new(percentage / 100.0)
        .map_err(|_| crate::ParseError::Opacity(unparsed.to_string()))
    } else {
      let value = unparsed
        .parse::<f32>()
        .map_err(|_| crate::ParseError::Opacity(unparsed.to_string()))?;
      Self::try_new(value)
        .map_err(|_| crate::ParseError::Opacity(unparsed.to_string()))
    }
  }
}

/// Deserialize an `OpacityValue` from either a number or a string.
impl<'de> Deserialize<'de> for OpacityValue {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    #[derive(Deserialize)]
    #[serde(untagged, rename_all = "camelCase")]
    enum OpacityValueDe {
      Number(f32),
      String(String),
    }

    match OpacityValueDe::deserialize(deserializer)? {
      OpacityValueDe::Number(num) => {
        Self::try_new(num).map_err(serde::de::Error::custom)
      }
      OpacityValueDe::String(str) => {
        Self::from_str(&str).map_err(serde::de::Error::custom)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalized_opacity_boundaries_are_accepted() {
    assert_eq!(OpacityValue::try_new(0.0).unwrap(), OpacityValue(0.0));
    assert_eq!(OpacityValue::try_new(1.0).unwrap(), OpacityValue(1.0));
    assert_eq!(OpacityValue::from_str("0%").unwrap(), OpacityValue(0.0));
    assert_eq!(OpacityValue::from_str("100%").unwrap(), OpacityValue(1.0));
  }

  #[test]
  fn invalid_normalized_opacity_is_rejected() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.1] {
      assert!(OpacityValue::try_new(value).is_err(), "accepted {value}");
    }

    for value in
      ["NaN", "inf", "-inf", "-0.1", "1.1", "-1%", "101%", "75%%"]
    {
      assert!(OpacityValue::from_str(value).is_err(), "accepted {value}");
    }
  }
}
