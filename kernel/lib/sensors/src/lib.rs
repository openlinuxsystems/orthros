// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The CPUs' temperature sensors (Phase 11e, ADR 0205), as Linux's
//! `k10temp` and `coretemp` drivers read them:
//!
//! - [`k10temp`]: AMD's Zen processors, whose System Management Unit
//!   (SMU) keeps the control temperature (Tctl) and each core complex
//!   die's (Tccd) in its thermal block, which the kernel reads over the
//!   System Management Network (SMN) through the root complex's index and
//!   data registers in PCI configuration space.
//! - [`coretemp`]: Intel's processors, whose digital thermal sensors count
//!   each core's and each package's temperature in degrees below the
//!   temperature their thermal control starts at (TjMax), in model-specific
//!   registers each CPU reads for itself.
//!
//! Temperatures are in thousandths of a degree Celsius, as Linux's hwmon
//! files give them. References: AMD's Processor Programming Reference for
//! family 17h, 19h and 1Ah (the SMN and the thermal block); Intel's SDM,
//! volume 3, section 15.8 (Platform Specific Power Management Support,
//! the digital thermal sensors); and Linux's `k10temp.c`, `coretemp.c` and
//! `amd_node.c` as the reference for which registers each processor has
//! (no code taken).
//!
//! The crate has no `unsafe` code: the kernel gives it bus 0's
//! configuration space for [`k10temp`] and reads [`coretemp`]'s registers
//! on each CPU. Its tests run on the host, [`k10temp`]'s against a
//! simulated root complex and data fabric.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod coretemp;
pub mod k10temp;
