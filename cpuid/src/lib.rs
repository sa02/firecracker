// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

#![deny(missing_docs)]
//! Utility for configuring the CPUID (CPU identification) for the guest microVM.

extern crate kvm;
extern crate kvm_bindings;

use std::result;

use kvm::CpuId;

/// Contains helper methods for bit operations.
pub mod bit_helper;
mod brand_string;
/// Follows a C3 template in setting up the CPUID.
pub mod c3_template;
mod cpu_leaf;
/// Follows a T2 template in setting up the CPUID.
pub mod t2_template;

use bit_helper::BitHelper;
use brand_string::BrandString;
use brand_string::Reg as BsReg;
use cpu_leaf::*;

/// Errors associated with configuring the CPUID entries.
#[derive(Debug)]
pub enum Error {
    /// The maximum number of addressable logical CPUs cannot be stored in an `u8`.
    VcpuCountOverflow,
    /// Failure with getting brand string.
    CreateBrandString(brand_string::Error),
}

/// Type for returning functions outcome.
pub type Result<T> = result::Result<T, Error>;

/// Type for propagating errors that can be optionally handled.
pub type PartialError<T, E> = (T, Option<E>);

/// Sets up the CPUID entries for the given vcpu.
///
/// # Arguments
///
/// * `cpu_id` - The index of the VCPU for which the CPUID entries are configured.
/// * `cpu_count` - The total number of present VCPUs.
/// * `ht_enabled` - Whether or not to enable HT.
/// * `kvm_cpuid` - KVM related structure holding the relevant CPUID info.
///
/// # Example
/// ```
/// extern crate cpuid;
/// extern crate kvm;
///
/// use cpuid::filter_cpuid;
/// use kvm::{CpuId, Kvm, MAX_KVM_CPUID_ENTRIES};
///
/// let kvm = Kvm::new().unwrap();
/// let mut kvm_cpuid: CpuId = kvm.get_supported_cpuid(MAX_KVM_CPUID_ENTRIES).unwrap();
/// filter_cpuid(0, 1, true, &mut kvm_cpuid).unwrap();
///
/// // Get expected `kvm_cpuid` entries.
/// let entries = kvm_cpuid.mut_entries_slice();
/// ```
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn filter_cpuid(
    cpu_id: u8,
    cpu_count: u8,
    ht_enabled: bool,
    kvm_cpuid: &mut CpuId,
) -> Result<()> {
    let entries = kvm_cpuid.mut_entries_slice();
    let max_addr_cpu = get_max_addressable_lprocessors(cpu_count)? as u32;

    let res = get_brand_string();
    let bstr = res.0;

    for entry in entries.iter_mut() {
        match entry.function {
            0x1 => {
                use leaf_0x1::*;

                // X86 hypervisor feature
                entry
                    .ecx
                    .write_bit(ecx::TSC_DEADLINE_TIMER_BITINDEX, true)
                    .write_bit(ecx::HYPERVISOR_BITINDEX, true);

                entry
                    .ebx
                    .write_bits_in_range(&ebx::APICID_BITRANGE, cpu_id as u32)
                    .write_bits_in_range(&ebx::CLFLUSH_SIZE_BITRANGE, EBX_CLFLUSH_CACHELINE)
                    .write_bits_in_range(&ebx::CPU_COUNT_BITRANGE, max_addr_cpu);

                // A value of 1 for HTT indicates the value in CPUID.1.EBX[23:16]
                // (the Maximum number of addressable IDs for logical processors in this package)
                // is valid for the package
                entry.edx.write_bit(edx::HTT, cpu_count > 1);
            }
            0x4 => {
                // Deterministic Cache Parameters Leaf
                use leaf_0x4::*;

                match entry.eax.read_bits_in_range(&eax::CACHE_LEVEL_BITRANGE) {
                    // L1 & L2 Cache
                    1 | 2 => {
                        // The L1 & L2 cache is shared by at most 2 hyperthreads
                        entry.eax.write_bits_in_range(
                            &eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE,
                            (cpu_count > 1 && ht_enabled) as u32,
                        );
                    }
                    // L3 Cache
                    3 => {
                        // The L3 cache is shared among all the logical threads
                        entry.eax.write_bits_in_range(
                            &eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE,
                            (cpu_count - 1) as u32,
                        );
                    }
                    _ => (),
                }

                // Put all the cores in the same socket
                entry.eax.write_bits_in_range(
                    &eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE,
                    (cpu_count - 1) as u32,
                );
            }
            0x6 => {
                use leaf_0x6::*;

                // Disable Turbo Boost
                entry.eax.write_bit(eax::TURBO_BOOST_BITINDEX, false);
                // Clear X86 EPB feature. No frequency selection in the hypervisor.
                entry.ecx.write_bit(ecx::EPB_BITINDEX, false);
            }
            0xA => {
                // Architectural Performance Monitor Leaf
                // Disable PMU
                entry.eax = 0;
                entry.ebx = 0;
                entry.ecx = 0;
                entry.edx = 0;
            }
            0xB => {
                use leaf_0xb::*;
                //reset eax, ebx, ecx
                entry.eax = 0 as u32;
                entry.ebx = 0 as u32;
                entry.ecx = 0 as u32;
                // EDX bits 31..0 contain x2APIC ID of current logical processor
                // x2APIC increases the size of the APIC ID from 8 bits to 32 bits
                entry.edx = cpu_id as u32;
                match entry.index {
                    // Thread Level Topology; index = 0
                    0 => {
                        // To get the next level APIC ID, shift right with at most 1 because we have
                        // maximum 2 hyperthreads per core that can be represented by 1 bit.
                        entry.eax.write_bits_in_range(
                            &eax::APICID_BITRANGE,
                            (cpu_count > 1 && ht_enabled) as u32,
                        );
                        // When cpu_count == 1 or HT is disabled, there is 1 logical core at this level
                        // Otherwise there are 2
                        entry.ebx.write_bits_in_range(
                            &ebx::NUM_LOGICAL_PROCESSORS_BITRANGE,
                            1 + (cpu_count > 1 && ht_enabled) as u32,
                        );

                        entry.ecx.write_bits_in_range(&ecx::LEVEL_TYPE_BITRANGE, {
                            if cpu_count == 1 {
                                // There are no hyperthreads for 1 VCPU, set the level type = 2 (Core)
                                LEVEL_TYPE_CORE
                            } else {
                                LEVEL_TYPE_THREAD
                            }
                        });
                    }
                    // Core Level Processor Topology; index = 1
                    1 => {
                        entry
                            .eax
                            .write_bits_in_range(&eax::APICID_BITRANGE, LEAFBH_INDEX1_APICID);
                        entry
                            .ecx
                            .write_bits_in_range(&ecx::LEVEL_NUMBER_BITRANGE, entry.index as u32);
                        if cpu_count == 1 {
                            // For 1 vCPU, this level is invalid
                            entry
                                .ecx
                                .write_bits_in_range(&ecx::LEVEL_TYPE_BITRANGE, LEVEL_TYPE_INVALID);
                        } else {
                            entry.ebx.write_bits_in_range(
                                &ebx::NUM_LOGICAL_PROCESSORS_BITRANGE,
                                cpu_count as u32,
                            );
                            entry
                                .ecx
                                .write_bits_in_range(&ecx::LEVEL_TYPE_BITRANGE, LEVEL_TYPE_CORE);
                        }
                    }
                    // Core Level Processor Topology; index >=2
                    // No other levels available; This should already be set to correctly,
                    // and it is added here as a "re-enforcement" in case we run on
                    // different hardware
                    level => {
                        entry.ecx = level;
                    }
                }
            }
            0x80000002..=0x80000004 => {
                entry.eax = bstr.get_reg_for_leaf(entry.function, BsReg::EAX);
                entry.ebx = bstr.get_reg_for_leaf(entry.function, BsReg::EBX);
                entry.ecx = bstr.get_reg_for_leaf(entry.function, BsReg::ECX);
                entry.edx = bstr.get_reg_for_leaf(entry.function, BsReg::EDX);
            }
            _ => (),
        }
    }

    if let Some(e) = res.1 {
        Err(Error::CreateBrandString(e))
    } else {
        Ok(())
    }
}

// constants for setting the fields of kvm_cpuid2 structures
// CPUID bits in ebx, ecx, and edx.
const EBX_CLFLUSH_CACHELINE: u32 = 8; // Flush a cache line size.

// The APIC ID shift in leaf 0xBh specifies the number of bits to shit the x2APIC ID to get a
// unique topology of the next level. This allows 64 logical processors/package.
const LEAFBH_INDEX1_APICID: u32 = 6;

const DEFAULT_BRAND_STRING: &[u8] = b"Intel(R) Xeon(R) Processor";

/// Sets leaf 01H EBX[23-16].
///
/// The maximum number of addressable logical CPUs is computed as the closest power of 2
/// higher or equal to the CPU count configured by the user.
fn get_max_addressable_lprocessors(cpu_count: u8) -> Result<u8> {
    let mut max_addressable_lcpu = (cpu_count as f64).log2().ceil();
    max_addressable_lcpu = (2 as f64).powf(max_addressable_lcpu);
    // check that this number is still an u8
    if max_addressable_lcpu > u8::max_value().into() {
        return Err(Error::VcpuCountOverflow);
    }
    Ok(max_addressable_lcpu as u8)
}

/// Generates the emulated brand string.
/// TODO: Add non-Intel CPU support.
///
/// For non-Intel CPUs, we'll just expose DEFAULT_BRAND_STRING.
///
/// For Intel CPUs, the brand string we expose will be:
///    "Intel(R) Xeon(R) Processor @ {host freq}"
/// where {host freq} is the CPU frequency, as present in the
/// host brand string (e.g. 4.01GHz).
///
/// This is safe because we know DEFAULT_BRAND_STRING to hold valid data
/// (allowed length and holding only valid ASCII chars).
fn get_brand_string() -> PartialError<BrandString, brand_string::Error> {
    let mut bstr = BrandString::from_bytes_unchecked(DEFAULT_BRAND_STRING);
    if let Ok(host_bstr) = BrandString::from_host_cpuid() {
        if host_bstr.starts_with(b"Intel") {
            if let Some(freq) = host_bstr.find_freq() {
                let mut v3 = vec![];
                v3.extend_from_slice(" @ ".as_bytes());
                v3.extend_from_slice(freq);
                if let Err(e) = bstr.push_bytes(&v3) {
                    return (bstr, Some(e));
                }
            }
        }
    }
    (bstr, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm::{Kvm, MAX_KVM_CPUID_ENTRIES};
    use kvm_bindings::kvm_cpuid_entry2;

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_get_max_addressable_lprocessors() {
        assert_eq!(get_max_addressable_lprocessors(1).unwrap(), 1);
        assert_eq!(get_max_addressable_lprocessors(2).unwrap(), 2);
        assert_eq!(get_max_addressable_lprocessors(4).unwrap(), 4);
        assert_eq!(get_max_addressable_lprocessors(6).unwrap(), 8);
        assert!(get_max_addressable_lprocessors(u8::max_value()).is_err());
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_cpuid() {
        let kvm = Kvm::new().unwrap();
        let mut kvm_cpuid: CpuId = kvm.get_supported_cpuid(MAX_KVM_CPUID_ENTRIES).unwrap();
        filter_cpuid(0, 1, true, &mut kvm_cpuid).unwrap();

        let entries = kvm_cpuid.mut_entries_slice();
        // TODO: This should be tested as part of the CI; only check that the function result is ok
        // after moving this to the CI.
        // Test the extended topology. See:
        // https://www.scss.tcd.ie/~jones/CS4021/processor-identification-cpuid-instruction-note.pdf
        let leaf11_index0 = kvm_cpuid_entry2 {
            function: 11,
            index: 0,
            flags: 1,
            eax: 0,
            // no of hyperthreads/core
            ebx: 1,
            // ECX[15:8] = 2 (Core Level)
            ecx: leaf_0xb::LEVEL_TYPE_CORE << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index,
            // EDX = APIC ID = 0
            edx: 0,
            padding: [0, 0, 0],
        };
        assert!(entries.contains(&leaf11_index0));
        let leaf11_index1 = kvm_cpuid_entry2 {
            function: 11,
            index: 1,
            flags: 1,
            eax: LEAFBH_INDEX1_APICID,
            ebx: 0,
            ecx: 1, // ECX[15:8] = 0 (Invalid Level) & ECX[7:0] = 1 (Level Number)
            edx: 0, // EDX = APIC ID = 0
            padding: [0, 0, 0],
        };
        assert!(entries.contains(&leaf11_index1));
        let leaf11_index2 = kvm_cpuid_entry2 {
            function: 11,
            index: 2,
            flags: 1,
            eax: 0,
            ebx: 0, // nr of hyperthreads/core
            ecx: 2, // ECX[15:8] = 0 (Invalid Level) & ECX[7:0] = 2 (Level Number)
            edx: 0, // EDX = APIC ID = 0
            padding: [0, 0, 0],
        };
        assert!(entries.contains(&leaf11_index2));
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_filter_cpuid_1vcpu_ht_off() {
        let mut kvm_cpuid = CpuId::new(11);
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[0].function = 0x1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[1].function = 0x4;
            entries[1].eax = 0b10000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[2].function = 0x4;
            entries[2].eax = 0b100000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[3].function = 0x4;
            entries[3].eax = 0b1000000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[4].function = 0x4;
            entries[4].eax = 0b1100000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[5].function = 0x6;
            entries[5].eax = 1;
            entries[5].ecx = 1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[6].function = 0xA;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[7].function = 0xB;
            entries[7].index = 0;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[8].function = 0xB;
            entries[8].index = 1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[9].function = 0xB;
            entries[9].index = 2;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[10].function = 0x80000003;
        }
        filter_cpuid(0, 1, false, &mut kvm_cpuid).unwrap();
        let max_addr_cpu = get_max_addressable_lprocessors(1).unwrap() as u32;

        let cpuid_f1 = kvm_cpuid_entry2 {
            function: 1,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: (EBX_CLFLUSH_CACHELINE << leaf_0x1::ebx::CLFLUSH_SIZE_BITRANGE.lsb_index)
                | max_addr_cpu << leaf_0x1::ebx::CPU_COUNT_BITRANGE.lsb_index,
            ecx: 1 << leaf_0x1::ecx::TSC_DEADLINE_TIMER_BITINDEX
                | 1 << leaf_0x1::ecx::HYPERVISOR_BITINDEX,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[0], cpuid_f1);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b10000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index),
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[1], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b100000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index),
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[2], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b1000000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index),
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[3], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b1100000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index),
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[4], cpuid_f4);
        }
        let cpuid_f6 = kvm_cpuid_entry2 {
            function: 0x6,
            index: 0,
            flags: 0,
            eax: 1 & !(1 << leaf_0x6::eax::TURBO_BOOST_BITINDEX),
            ebx: 0,
            ecx: 1 & !(1 << leaf_0x6::ecx::EPB_BITINDEX),
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[5], cpuid_f6);
        }
        let cpuid_fa = kvm_cpuid_entry2 {
            function: 0xA,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[6], cpuid_fa);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 1,
            ecx: leaf_0xb::LEVEL_TYPE_CORE << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[7], cpuid_fb);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 1,
            flags: 0,
            eax: LEAFBH_INDEX1_APICID,
            ebx: 0,
            ecx: 1 | (leaf_0xb::LEVEL_TYPE_INVALID << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index),
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[8], cpuid_fb);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 2,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 2,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[9], cpuid_fb);
        }
        let bstr = get_brand_string().0;
        let cpuid_fother = kvm_cpuid_entry2 {
            function: 0x80000003,
            index: 0,
            flags: 0,
            eax: bstr.get_reg_for_leaf(0x80000003, BsReg::EAX),
            ebx: bstr.get_reg_for_leaf(0x80000003, BsReg::EBX),
            ecx: bstr.get_reg_for_leaf(0x80000003, BsReg::ECX),
            edx: bstr.get_reg_for_leaf(0x80000003, BsReg::EDX),
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[10], cpuid_fother);
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_filter_cpuid_multiple_vcpu_ht_off() {
        let mut kvm_cpuid = CpuId::new(11);
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[0].function = 0x1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[1].function = 0x4;
            entries[1].eax = 0b10000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[2].function = 0x4;
            entries[2].eax = 0b100000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[3].function = 0x4;
            entries[3].eax = 0b1000000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[4].function = 0x4;
            entries[4].eax = 0b1100000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[5].function = 0x6;
            entries[5].eax = 1;
            entries[5].ecx = 1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[6].function = 0xA;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[7].function = 0xB;
            entries[7].index = 0;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[8].function = 0xB;
            entries[8].index = 1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[9].function = 0xB;
            entries[9].index = 2;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[10].function = 0x80000003;
        }
        let cpu_count = 3;
        filter_cpuid(0, cpu_count, false, &mut kvm_cpuid).unwrap();
        let max_addr_cpu = get_max_addressable_lprocessors(cpu_count).unwrap() as u32;

        let cpuid_f1 = kvm_cpuid_entry2 {
            function: 1,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: (EBX_CLFLUSH_CACHELINE << leaf_0x1::ebx::CLFLUSH_SIZE_BITRANGE.lsb_index)
                | max_addr_cpu << leaf_0x1::ebx::CPU_COUNT_BITRANGE.lsb_index,
            ecx: 1 << leaf_0x1::ecx::TSC_DEADLINE_TIMER_BITINDEX
                | 1 << leaf_0x1::ecx::HYPERVISOR_BITINDEX,
            edx: 1 << leaf_0x1::edx::HTT,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[0], cpuid_f1);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b10000 & !(0b111111 << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[1], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b100000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index)
                & !(0b111111 << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[2], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b1000000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index)
                & !(0b111111 << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[3], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b1100000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index
                    & !(0b111111 << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[4], cpuid_f4);
        }
        let cpuid_f6 = kvm_cpuid_entry2 {
            function: 0x6,
            index: 0,
            flags: 0,
            eax: 1 & !(1 << leaf_0x6::eax::TURBO_BOOST_BITINDEX),
            ebx: 0,
            ecx: 1 & !(1 << leaf_0x6::ecx::EPB_BITINDEX),
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[5], cpuid_f6);
        }
        let cpuid_fa = kvm_cpuid_entry2 {
            function: 0xA,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[6], cpuid_fa);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 1,
            ecx: leaf_0xb::LEVEL_TYPE_THREAD << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[7], cpuid_fb);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 1,
            flags: 0,
            eax: LEAFBH_INDEX1_APICID,
            ebx: cpu_count as u32,
            ecx: 1 | leaf_0xb::LEVEL_TYPE_CORE << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[8], cpuid_fb);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 2,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 2,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[9], cpuid_fb);
        }
        let bstr = get_brand_string().0;
        let cpuid_fother = kvm_cpuid_entry2 {
            function: 0x80000003,
            index: 0,
            flags: 0,
            eax: bstr.get_reg_for_leaf(0x80000003, BsReg::EAX),
            ebx: bstr.get_reg_for_leaf(0x80000003, BsReg::EBX),
            ecx: bstr.get_reg_for_leaf(0x80000003, BsReg::ECX),
            edx: bstr.get_reg_for_leaf(0x80000003, BsReg::EDX),
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[10], cpuid_fother);
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_filter_cpuid_1vcpu_ht_on() {
        let mut kvm_cpuid = CpuId::new(11);
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[0].function = 0x1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[1].function = 0x4;
            entries[1].eax = 0b10000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[2].function = 0x4;
            entries[2].eax = 0b100000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[3].function = 0x4;
            entries[3].eax = 0b1000000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[4].function = 0x4;
            entries[4].eax = 0b1100000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[5].function = 0x6;
            entries[5].eax = 1;
            entries[5].ecx = 1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[6].function = 0xA;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[7].function = 0xB;
            entries[7].index = 0;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[8].function = 0xB;
            entries[8].index = 1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[9].function = 0xB;
            entries[9].index = 2;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[10].function = 0x80000003;
        }
        filter_cpuid(0, 1, true, &mut kvm_cpuid).unwrap();
        let max_addr_cpu = get_max_addressable_lprocessors(1).unwrap() as u32;

        let cpuid_f1 = kvm_cpuid_entry2 {
            function: 1,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: (EBX_CLFLUSH_CACHELINE << leaf_0x1::ebx::CLFLUSH_SIZE_BITRANGE.lsb_index)
                | max_addr_cpu << leaf_0x1::ebx::CPU_COUNT_BITRANGE.lsb_index,
            ecx: 1 << leaf_0x1::ecx::TSC_DEADLINE_TIMER_BITINDEX
                | 1 << leaf_0x1::ecx::HYPERVISOR_BITINDEX,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[0], cpuid_f1);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b10000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index),
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[1], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b100000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index),
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[2], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b1000000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index),
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[3], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b1100000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index),
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[4], cpuid_f4);
        }
        let cpuid_f6 = kvm_cpuid_entry2 {
            function: 0x6,
            index: 0,
            flags: 0,
            eax: 1 & !(1 << leaf_0x6::eax::TURBO_BOOST_BITINDEX),
            ebx: 0,
            ecx: 1 & !(1 << leaf_0x6::ecx::EPB_BITINDEX),
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[5], cpuid_f6);
        }
        let cpuid_fa = kvm_cpuid_entry2 {
            function: 0xA,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[6], cpuid_fa);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 1,
            ecx: leaf_0xb::LEVEL_TYPE_CORE << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[7], cpuid_fb);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 1,
            flags: 0,
            eax: LEAFBH_INDEX1_APICID,
            ebx: 0,
            ecx: 1 | (leaf_0xb::LEVEL_TYPE_INVALID << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index),
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[8], cpuid_fb);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 2,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 2,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[9], cpuid_fb);
        }
        let bstr = get_brand_string().0;
        let cpuid_fother = kvm_cpuid_entry2 {
            function: 0x80000003,
            index: 0,
            flags: 0,
            eax: bstr.get_reg_for_leaf(0x80000003, BsReg::EAX),
            ebx: bstr.get_reg_for_leaf(0x80000003, BsReg::EBX),
            ecx: bstr.get_reg_for_leaf(0x80000003, BsReg::ECX),
            edx: bstr.get_reg_for_leaf(0x80000003, BsReg::EDX),
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[10], cpuid_fother);
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn test_filter_cpuid_multiple_vcpu_ht_on() {
        let mut kvm_cpuid = CpuId::new(11);
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[0].function = 0x1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[1].function = 0x4;
            entries[1].eax = 0b10000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[2].function = 0x4;
            entries[2].eax = 0b100000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[3].function = 0x4;
            entries[3].eax = 0b1000000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[4].function = 0x4;
            entries[4].eax = 0b1100000;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[5].function = 0x6;
            entries[5].eax = 1;
            entries[5].ecx = 1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[6].function = 0xA;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[7].function = 0xB;
            entries[7].index = 0;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[8].function = 0xB;
            entries[8].index = 1;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[9].function = 0xB;
            entries[9].index = 2;
        }
        {
            let entries = kvm_cpuid.mut_entries_slice();
            entries[10].function = 0x80000003;
        }
        let cpu_count = 3;
        filter_cpuid(0, cpu_count, true, &mut kvm_cpuid).unwrap();
        let max_addr_cpu = get_max_addressable_lprocessors(cpu_count).unwrap() as u32;

        let cpuid_f1 = kvm_cpuid_entry2 {
            function: 1,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: (EBX_CLFLUSH_CACHELINE << leaf_0x1::ebx::CLFLUSH_SIZE_BITRANGE.lsb_index)
                | max_addr_cpu << leaf_0x1::ebx::CPU_COUNT_BITRANGE.lsb_index,
            ecx: 1 << leaf_0x1::ecx::TSC_DEADLINE_TIMER_BITINDEX
                | 1 << leaf_0x1::ecx::HYPERVISOR_BITINDEX,
            edx: 1 << leaf_0x1::edx::HTT,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[0], cpuid_f1);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b10000 & !(0b111111 << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[1], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b100000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index)
                | 1 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index
                    & !(0b111111 << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[2], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b1000000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index)
                | 1 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index
                    & !(0b111111 << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[3], cpuid_f4);
        }
        let cpuid_f4 = kvm_cpuid_entry2 {
            function: 0x4,
            index: 0,
            flags: 0,
            eax: 0b1100000
                & !(0b111111111111 << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_SHARING_CACHE_BITRANGE.lsb_index
                    & !(0b111111 << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index)
                | ((cpu_count - 1) as u32)
                    << leaf_0x4::eax::MAX_ADDR_IDS_IN_PACKAGE_BITRANGE.lsb_index,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[4], cpuid_f4);
        }
        let cpuid_f6 = kvm_cpuid_entry2 {
            function: 0x6,
            index: 0,
            flags: 0,
            eax: 1 & !(1 << leaf_0x6::eax::TURBO_BOOST_BITINDEX),
            ebx: 0,
            ecx: 1 & !(1 << leaf_0x6::ecx::EPB_BITINDEX),
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[5], cpuid_f6);
        }
        let cpuid_fa = kvm_cpuid_entry2 {
            function: 0xA,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[6], cpuid_fa);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 0,
            flags: 0,
            eax: 1,
            ebx: 2,
            ecx: leaf_0xb::LEVEL_TYPE_THREAD << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[7], cpuid_fb);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 1,
            flags: 0,
            eax: LEAFBH_INDEX1_APICID,
            ebx: cpu_count as u32,
            ecx: 1 | leaf_0xb::LEVEL_TYPE_CORE << leaf_0xb::ecx::LEVEL_TYPE_BITRANGE.lsb_index,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[8], cpuid_fb);
        }
        let cpuid_fb = kvm_cpuid_entry2 {
            function: 0xB,
            index: 2,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 2,
            edx: 0,
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[9], cpuid_fb);
        }
        let bstr = get_brand_string().0;
        let cpuid_fother = kvm_cpuid_entry2 {
            function: 0x80000003,
            index: 0,
            flags: 0,
            eax: bstr.get_reg_for_leaf(0x80000003, BsReg::EAX),
            ebx: bstr.get_reg_for_leaf(0x80000003, BsReg::EBX),
            ecx: bstr.get_reg_for_leaf(0x80000003, BsReg::ECX),
            edx: bstr.get_reg_for_leaf(0x80000003, BsReg::EDX),
            padding: [0, 0, 0],
        };
        {
            let entries = kvm_cpuid.mut_entries_slice();
            assert_eq!(entries[10], cpuid_fother);
        }
    }
}
