// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use std::error;
use std::fmt;

pub use crate::autogen::Features;

/// An error that can happen when enabling a feature on a device.
#[derive(Clone, Copy, Debug)]
pub struct FeatureRestrictionError {
    /// The feature in question.
    pub feature: &'static str,
    /// The restriction that was not met.
    pub restriction: FeatureRestriction,
}

impl error::Error for FeatureRestrictionError {}

impl fmt::Display for FeatureRestrictionError {
    #[inline]
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(
            fmt,
            "a restriction for the feature {} was not met: {}",
            self.feature, self.restriction,
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub enum FeatureRestriction {
    /// Not supported by the physical device.
    NotSupported,
    /// Requires a feature to be enabled.
    RequiresFeature(&'static str),
    /// Requires a feature to be disabled.
    ConflictsFeature(&'static str),
    /// An extension requires this feature to be enabled.
    RequiredByExtension(&'static str),
}

impl fmt::Display for FeatureRestriction {
    #[inline]
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            FeatureRestriction::NotSupported => {
                write!(fmt, "not supported by the physical device")
            }
            FeatureRestriction::RequiresFeature(feat) => {
                write!(fmt, "requires feature {} to be enabled", feat)
            }
            FeatureRestriction::ConflictsFeature(feat) => {
                write!(fmt, "requires feature {} to be disabled", feat)
            }
            FeatureRestriction::RequiredByExtension(ext) => {
                write!(fmt, "required to be enabled by extension {}", ext)
            }
        }
    }
}

pub(crate) use crate::autogen::FeaturesFfi;
