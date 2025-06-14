#![doc = include_str!("../README.md")]
#![feature(stdarch_x86_avx512)]
use core::arch::x86_64::*;
use core::hint::unreachable_unchecked;

#[cfg(feature = "client")]
pub mod client;

mod sha256;

#[cfg(feature = "wgpu")]
pub mod wgpu;

const SWAP_DWORD_BYTE_ORDER: [usize; 64] = [
    3, 2, 1, 0, 7, 6, 5, 4, 11, 10, 9, 8, 15, 14, 13, 12, 19, 18, 17, 16, 23, 22, 21, 20, 27, 26,
    25, 24, 31, 30, 29, 28, 35, 34, 33, 32, 39, 38, 37, 36, 43, 42, 41, 40, 47, 46, 45, 44, 51, 50,
    49, 48, 55, 54, 53, 52, 59, 58, 57, 56, 63, 62, 61, 60,
];

#[cfg(feature = "bincode")]
pub fn build_prefix<W: std::io::Write>(
    out: &mut W,
    string: &str,
    salt: &str,
) -> std::io::Result<()> {
    out.write_all(salt.as_bytes())?;
    match bincode::serialize_into(out, string) {
        Ok(_) => (),
        Err(e) => match *e {
            bincode::ErrorKind::Io(e) => return Err(e),
            _ => unreachable!(),
        },
    };
    Ok(())
}

pub const fn decompose_blocks(inp: &[u32; 16]) -> &[u8; 64] {
    unsafe { core::mem::transmute(inp) }
}

pub const fn decompose_blocks_mut(inp: &mut [u32; 16]) -> &mut [u8; 64] {
    unsafe { core::mem::transmute(inp) }
}

pub const fn compute_target(difficulty_factor: u32) -> u128 {
    u128::max_value() - u128::max_value() / difficulty_factor as u128
}

pub trait Solver {
    type Ctx;

    // construct a new solver instance from a prefix
    // prefix is the message that precedes the N in the single block of SHA-256 message
    // in mCaptcha it is the bincode serialized message then immediately the salt
    //
    // returns None when this solver cannot solve the prefix
    fn new(ctx: Self::Ctx, prefix: &[u8]) -> Option<Self>
    where
        Self: Sized;

    // returns a valid nonce and "result" value
    //
    // returns None when the solver cannot solve the prefix
    // failure is usually because the key space is exhausted (or presumed exhausted) and happens extremely rarely for common difficulty settings
    fn solve(&mut self, target: [u32; 4]) -> Option<(u64, u128)>;
}

// Solves an mCaptcha SHA256 PoW where the SHA-256 message is a single block (512 bytes minus padding).
//
// There is currently no AVX2 fallback for more common hardware
#[derive(Debug, Clone)]
pub struct SingleBlockSolver16Way {
    // the SHA-256 state A-H for all prefix bytes
    pub(crate) prefix_state: [u32; 8],

    // the message template for the final block
    pub(crate) message: [u32; 16],

    pub(crate) digit_index: usize,

    pub(crate) nonce_addend: u64,
}

impl Solver for SingleBlockSolver16Way {
    type Ctx = ();

    fn new(_ctx: Self::Ctx, mut prefix: &[u8]) -> Option<Self> {
        // construct the message buffer
        let mut prefix_state = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut nonce_addend = 0u64;
        let mut complete_blocks_before = 0;

        // first consume all full blocks, this is shared so use scalar reference implementation
        while prefix.len() >= 64 {
            sha256::compress_block_reference(
                &mut prefix_state,
                &core::array::from_fn(|i| {
                    u32::from_be_bytes([
                        prefix[i * 4],
                        prefix[i * 4 + 1],
                        prefix[i * 4 + 2],
                        prefix[i * 4 + 3],
                    ])
                }),
            );
            prefix = &prefix[64..];
            complete_blocks_before += 1;
        }
        // if there is not enough room for 9 bytes of padding, '1's and then start a new block whenever possible
        // this avoids having to hash 2 blocks per iteration a naive solution would do
        if prefix.len() + 9 + 9 > 64 {
            let mut tmp_block = [0; 64];
            tmp_block[..prefix.len()].copy_from_slice(prefix);
            tmp_block[prefix.len()..].iter_mut().for_each(|b| {
                nonce_addend *= 10;
                nonce_addend += 1;
                *b = b'1';
            });
            nonce_addend = nonce_addend.checked_mul(1_000_000_000)?;
            complete_blocks_before += 1;
            prefix = &[];
            sha256::compress_block_reference(
                &mut prefix_state,
                &core::array::from_fn(|i| {
                    u32::from_be_bytes([
                        tmp_block[i * 4],
                        tmp_block[i * 4 + 1],
                        tmp_block[i * 4 + 2],
                        tmp_block[i * 4 + 3],
                    ])
                }),
            );
        }

        let mut message: [u8; 64] = [0; 64];
        let mut ptr = 0;
        message[..prefix.len()].copy_from_slice(prefix);
        ptr += prefix.len();
        let digit_index = ptr;

        // skip 9 zeroes, this is the part we will interpolate N into
        // the first 2 digits are used as the lane index (10 + (0..16)*(0..4), offset to avoid leading zeroes), this also keeps our proof plausible
        // the rest are randomly generated then broadcasted to all lanes
        // this gives us about 16e7 * 4 possible attempts, likely enough for any realistic deployment even on the highest difficulty
        // the fail rate would be pgeom(keySpace, 1/difficulty, lower=F) in R
        ptr += 9;

        // set up padding
        message[ptr] = 0x80;
        message[(64 - 8)..]
            .copy_from_slice(&((complete_blocks_before * 64 + ptr) as u64 * 8).to_be_bytes());

        Some(Self {
            prefix_state,
            message: core::array::from_fn(|i| {
                u32::from_be_bytes([
                    message[i * 4],
                    message[i * 4 + 1],
                    message[i * 4 + 2],
                    message[i * 4 + 3],
                ])
            }),
            digit_index,
            nonce_addend,
        })
    }

    fn solve(&mut self, target: [u32; 4]) -> Option<(u64, u128)> {
        // the official default difficulty is 5e6, so we design for 1e8
        // and there should almost always be a valid solution within our supported solution space
        // pgeom(5 * 16e7, 1/5e7, lower=F) = 0.03%
        // pgeom(16e7, 1/5e7, lower=F) = 20%, which is too much so we need the prefix to change as well

        // pre-compute an OR to apply to the message to add the lane ID
        let lane_id_0_word_idx = self.digit_index / 4;
        let lane_id_1_word_idx = (self.digit_index + 1) / 4;

        // make sure there are no runtime "register indexing" logic
        fn solve_inner<const DIGIT_WORD_IDX0: usize, const DIGIT_WORD_IDX1: usize>(
            this: &mut SingleBlockSolver16Way,
            target: u32,
        ) -> Option<u64> {
            let lane_id_0_byte_idx = this.digit_index % 4;
            let lane_id_1_byte_idx = (this.digit_index + 1) % 4;
            // pre-compute the lane index OR mask to "stamp" onto each lane for each try
            // this string is longer than we need but good enough for all intents and purposes
            let lane_id_0_or_value: [u32; 5 * 16] = core::array::from_fn(|i| {
                (b"111111111122222222223333333333444444444455555555556666666666777777777788888888889999999999"[i] as u32) << ((3 - lane_id_0_byte_idx) * 8) as u32
            });

            let lane_id_1_or_value: [u32; 5 * 16] = core::array::from_fn(|i| {
                (b"012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789"[i] as u32) << ((3 - lane_id_1_byte_idx) * 8) as u32
            });

            for prefix_set_index in 0..5 {
                let lane_id_0_or_value_v = unsafe {
                    if DIGIT_WORD_IDX0 == DIGIT_WORD_IDX1 {
                        _mm512_or_epi32(
                            _mm512_loadu_epi32(
                                lane_id_0_or_value
                                    .as_ptr()
                                    .add(prefix_set_index as usize * 16)
                                    .cast(),
                            ),
                            _mm512_loadu_epi32(
                                lane_id_1_or_value
                                    .as_ptr()
                                    .add(prefix_set_index as usize * 16)
                                    .cast(),
                            ),
                        )
                    } else {
                        _mm512_loadu_epi32(
                            lane_id_0_or_value
                                .as_ptr()
                                .add(prefix_set_index as usize * 16)
                                .cast(),
                        )
                    }
                };
                let lane_id_1_or_value_v = unsafe {
                    _mm512_loadu_epi32(
                        lane_id_1_or_value
                            .as_ptr()
                            .add(prefix_set_index as usize * 16)
                            .cast(),
                    )
                };
                macro_rules! fetch_msg {
                    ($idx:expr) => {
                        if $idx == DIGIT_WORD_IDX0 {
                            _mm512_or_epi32(
                                _mm512_set1_epi32(this.message[$idx] as _),
                                lane_id_0_or_value_v,
                            )
                        } else if $idx == DIGIT_WORD_IDX1 {
                            _mm512_or_epi32(
                                _mm512_set1_epi32(this.message[$idx] as _),
                                lane_id_1_or_value_v,
                            )
                        } else {
                            _mm512_set1_epi32(this.message[$idx] as _)
                        }
                    };
                }

                let mut blocks = unsafe {
                    [
                        fetch_msg!(0),
                        fetch_msg!(1),
                        fetch_msg!(2),
                        fetch_msg!(3),
                        fetch_msg!(4),
                        fetch_msg!(5),
                        fetch_msg!(6),
                        fetch_msg!(7),
                        fetch_msg!(8),
                        fetch_msg!(9),
                        fetch_msg!(10),
                        fetch_msg!(11),
                        fetch_msg!(12),
                        fetch_msg!(13),
                        fetch_msg!(14),
                        fetch_msg!(15),
                    ]
                };

                for inner_key in 0..10_000_000 {
                    unsafe {
                        let mut key_copy = inner_key;
                        {
                            let message_bytes = decompose_blocks_mut(&mut this.message);

                            for i in (0..7).rev() {
                                let output = key_copy % 10;
                                key_copy /= 10;
                                *message_bytes.get_unchecked_mut(
                                    *SWAP_DWORD_BYTE_ORDER.get_unchecked(this.digit_index + i + 2),
                                ) = output as u8 + b'0';
                            }
                        }
                        debug_assert_eq!(key_copy, 0);

                        // we need to re-load at least 2 blocks and at most 3 blocks
                        blocks[DIGIT_WORD_IDX1] = fetch_msg!(DIGIT_WORD_IDX1);
                        if DIGIT_WORD_IDX1 < 15 {
                            blocks[DIGIT_WORD_IDX1 + 1] = fetch_msg!(DIGIT_WORD_IDX1 + 1);
                        }
                        if DIGIT_WORD_IDX1 < 14 {
                            blocks[DIGIT_WORD_IDX1 + 2] = fetch_msg!(DIGIT_WORD_IDX1 + 2);
                        }

                        let mut state =
                            core::array::from_fn(|i| _mm512_set1_epi32(this.prefix_state[i] as _));

                        // do 16-way SHA-256 without feedback so as not to force the compiler to save 8 registers
                        // we already have them in scalar form, this allows more registers to be reused in the next iteration
                        sha256::compress_16block_avx512_without_feedback(&mut state, &mut blocks);

                        // the target is big endian interpretation of the first 16 bytes of the hash (A-D) >= target
                        // however, the largest 32-bit digits is unlikely to be all ones (otherwise a legitimate challenger needs on average >2^32 attempts)
                        // so we can reduce this into simply testing H[0]
                        // the number of acceptable u32 values (for us) is u32::MAX / difficulty
                        // so the "inefficiency" this creates is about (u32::MAX / difficulty) * (1 / 2), because for approx. half of the "edge case" do we actually have an acceptable solution,
                        // which for 1e8 is about 1%, but we get to save the one broadcast add,
                        // a vectorized comparison, and a scalar logic evaluation
                        // which I feel is about 1% of the instructions needed per iteration anyways just more registers used so let's not bother
                        let a_is_greater = _mm512_cmpgt_epu32_mask(
                            _mm512_add_epi32(
                                state[0],
                                _mm512_set1_epi32(this.prefix_state[0] as _),
                            ),
                            _mm512_set1_epi32(target as _),
                        );

                        if a_is_greater != 0 {
                            let success_lane_idx = _tzcnt_u32(a_is_greater as _) as usize;
                            let nonce_prefix = 10 + 16 * prefix_set_index + success_lane_idx as u64;

                            // stamp the lane ID back onto the message
                            {
                                let message_bytes = decompose_blocks_mut(&mut this.message);
                                *message_bytes.get_unchecked_mut(
                                    *SWAP_DWORD_BYTE_ORDER.get_unchecked(this.digit_index),
                                ) = (nonce_prefix / 10) as u8 + b'0';
                                *message_bytes.get_unchecked_mut(
                                    *SWAP_DWORD_BYTE_ORDER.get_unchecked(this.digit_index + 1),
                                ) = (nonce_prefix % 10) as u8 + b'0';
                            }

                            // the nonce is the 7 digits in the message, plus the first two digits recomputed from the lane index
                            return Some(nonce_prefix * 10u64.pow(7) + inner_key);
                        }
                    }
                }
            }
            None
        }

        macro_rules! dispatch {
            ($idx0:literal) => {
                match lane_id_1_word_idx {
                    0 => solve_inner::<$idx0, 0>(self, target[0]),
                    1 => solve_inner::<$idx0, 1>(self, target[0]),
                    2 => solve_inner::<$idx0, 2>(self, target[0]),
                    3 => solve_inner::<$idx0, 3>(self, target[0]),
                    4 => solve_inner::<$idx0, 4>(self, target[0]),
                    5 => solve_inner::<$idx0, 5>(self, target[0]),
                    6 => solve_inner::<$idx0, 6>(self, target[0]),
                    7 => solve_inner::<$idx0, 7>(self, target[0]),
                    8 => solve_inner::<$idx0, 8>(self, target[0]),
                    9 => solve_inner::<$idx0, 9>(self, target[0]),
                    10 => solve_inner::<$idx0, 10>(self, target[0]),
                    11 => solve_inner::<$idx0, 11>(self, target[0]),
                    12 => solve_inner::<$idx0, 12>(self, target[0]),
                    13 => solve_inner::<$idx0, 13>(self, target[0]),
                    14 => solve_inner::<$idx0, 14>(self, target[0]),
                    15 => solve_inner::<$idx0, 15>(self, target[0]),
                    _ => unreachable_unchecked(),
                }
            };
        }

        let nonce = unsafe {
            match lane_id_0_word_idx {
                0 => dispatch!(0),
                1 => dispatch!(1),
                2 => dispatch!(2),
                3 => dispatch!(3),
                4 => dispatch!(4),
                5 => dispatch!(5),
                6 => dispatch!(6),
                7 => dispatch!(7),
                8 => dispatch!(8),
                9 => dispatch!(9),
                10 => dispatch!(10),
                11 => dispatch!(11),
                12 => dispatch!(12),
                13 => dispatch!(13),
                14 => dispatch!(14),
                15 => dispatch!(15),
                _ => unreachable_unchecked(),
            }
        }?;

        // recompute the hash from the beginning
        // this prevents the compiler from having to compute the final B-H registers alive in tight loops
        let mut final_sha_state = self.prefix_state.clone();
        sha256::compress_block_reference(&mut final_sha_state, &self.message);

        Some((
            nonce + self.nonce_addend,
            (final_sha_state[0] as u128) << 96
                | (final_sha_state[1] as u128) << 64
                | (final_sha_state[2] as u128) << 32
                | (final_sha_state[3] as u128),
        ))
    }
}

/// Solver for double SHA-256 cases
///
/// It has slightly better than half throughput than the single block solver, but you should use the single block solver if possible
pub struct DoubleBlockSolver16Way {
    // the SHA-256 state A-H for all prefix bytes
    pub(crate) prefix_state: [u32; 8],

    // the message template for the final block
    pub(crate) message: [u32; 16],

    // the pre-computed message schedule for the padding block (i.e. zeroes then finally the length)
    pub(crate) terminal_message_schedule: [u32; 64],

    pub(crate) nonce_addend: u64,
}

impl DoubleBlockSolver16Way {
    const DIGIT_IDX: u64 = 54;
}

impl Solver for DoubleBlockSolver16Way {
    type Ctx = ();

    fn new(_ctx: Self::Ctx, mut prefix: &[u8]) -> Option<Self>
    where
        Self: Sized,
    {
        // construct the message buffer
        let mut prefix_state = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];

        let mut complete_blocks_before = 0;

        // first consume all full blocks, this is shared so use scalar reference implementation
        while prefix.len() >= 64 {
            sha256::compress_block_reference(
                &mut prefix_state,
                &core::array::from_fn(|i| {
                    u32::from_be_bytes([
                        prefix[i * 4],
                        prefix[i * 4 + 1],
                        prefix[i * 4 + 2],
                        prefix[i * 4 + 3],
                    ])
                }),
            );
            prefix = &prefix[64..];
            complete_blocks_before += 1;
        }

        let mut message: [u8; 64] = [0; 64];
        let mut ptr = 0;
        message[..prefix.len()].copy_from_slice(prefix);
        ptr += prefix.len();

        // pad with ones until we are on a 64-bit boundary minus 2 byte
        // we have much more leeway here as we are committed to a double block solver, using more bytes is fine, there is nothing useful to be traded off
        let mut nonce_addend = 0;
        while (ptr + 2) % 8 != 0 {
            nonce_addend *= 10;
            nonce_addend += 1;
            *message.get_mut(ptr)? = b'1';
            ptr += 1;
        }
        nonce_addend *= 1_000_000_000;

        // these cases are handled by the single block solver
        if ptr != Self::DIGIT_IDX as usize {
            return None;
        }

        // skip 9 zeroes, this is the part we will interpolate N into
        // the first 2 digits are used as the lane index (10 + (0..16)*(0..4), offset to avoid leading zeroes)
        // the rest are randomly generated then broadcasted to all lanes
        // this gives us about 16e7 * 4 possible attempts, likely enough for any realistic deployment even on the highest difficulty
        // the fail rate would be pgeom(keySpace, 1/difficulty, lower=F) in R
        ptr += 9;

        // we should be at the end of the message buffer minus 1
        debug_assert_eq!(ptr, 63);

        message[ptr] = 0x80;

        let message_length = complete_blocks_before * 64 + ptr;

        let mut terminal_message_schedule = [0; 64];
        terminal_message_schedule[14] = ((message_length * 8) >> 32) as u32;
        terminal_message_schedule[15] = (message_length * 8) as u32;

        sha256::do_message_schedule(&mut terminal_message_schedule);

        Some(Self {
            prefix_state,
            message: core::array::from_fn(|i| {
                u32::from_be_bytes([
                    message[i * 4],
                    message[i * 4 + 1],
                    message[i * 4 + 2],
                    message[i * 4 + 3],
                ])
            }),
            terminal_message_schedule,
            nonce_addend,
        })
    }

    fn solve(&mut self, target: [u32; 4]) -> Option<(u64, u128)> {
        let lane_id_0_byte_idx = Self::DIGIT_IDX % 4;
        let lane_id_1_byte_idx = (Self::DIGIT_IDX + 1) % 4;
        // pre-compute the lane index OR mask to "stamp" onto each lane for each try
        // this string is longer than we need but good enough for all intents and purposes
        let lane_id_or_value: [u32; 5 * 16] = core::array::from_fn(|i| {
            let lane_0 = (b"111111111122222222223333333333444444444455555555556666666666777777777788888888889999999999"[i] as u32) << ((3 - lane_id_0_byte_idx) * 8) as u32;
            let lane_1 = (b"012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789"[i] as u32) << ((3 - lane_id_1_byte_idx) * 8) as u32;
            lane_0 | lane_1
        });

        let mut blocks = unsafe {
            [
                _mm512_set1_epi32(self.message[0] as _),
                _mm512_set1_epi32(self.message[1] as _),
                _mm512_set1_epi32(self.message[2] as _),
                _mm512_set1_epi32(self.message[3] as _),
                _mm512_set1_epi32(self.message[4] as _),
                _mm512_set1_epi32(self.message[5] as _),
                _mm512_set1_epi32(self.message[6] as _),
                _mm512_set1_epi32(self.message[7] as _),
                _mm512_set1_epi32(self.message[8] as _),
                _mm512_set1_epi32(self.message[9] as _),
                _mm512_set1_epi32(self.message[10] as _),
                _mm512_set1_epi32(self.message[11] as _),
                _mm512_set1_epi32(self.message[12] as _),
                _mm512_setzero_epi32(), // 13 is always zero for a valid construction
                _mm512_setzero_epi32(), // 14 is filled in later
                _mm512_setzero_epi32(), // 15 is filled in later
            ]
        };

        for prefix_set_index in 0..5 {
            unsafe {
                blocks[13] = _mm512_loadu_epi32(
                    lane_id_or_value
                        .as_ptr()
                        .add(prefix_set_index as usize * 16)
                        .cast(),
                )
            };

            for inner_key in 0..10_000_000 {
                unsafe {
                    let mut key_copy = inner_key;
                    let mut cum0 = 0;
                    for _ in 0..4 {
                        cum0 <<= 8;
                        cum0 |= key_copy % 10;
                        key_copy /= 10;
                    }
                    cum0 |= u32::from_be_bytes(*b"0000");
                    blocks[14] = _mm512_set1_epi32(cum0 as _);
                    let mut cum1 = 0;
                    for _ in 0..3 {
                        cum1 += key_copy % 10;
                        cum1 <<= 8;
                        key_copy /= 10;
                    }
                    cum1 |= u32::from_be_bytes(*b"000\x80");
                    blocks[15] = _mm512_set1_epi32(cum1 as _);

                    let mut state =
                        core::array::from_fn(|i| _mm512_set1_epi32(self.prefix_state[i] as _));

                    sha256::compress_16block_avx512_without_feedback(&mut state, &mut blocks);

                    // we have to do feedback now
                    for i in 0..8 {
                        state[i] = _mm512_add_epi32(
                            state[i],
                            _mm512_set1_epi32(self.prefix_state[i] as _),
                        );
                    }

                    // save only A register for comparison
                    let save_a = state[0];

                    sha256::compress_16block_avx512_bcst_without_feedback::<14>(
                        &mut state,
                        &self.terminal_message_schedule,
                    );

                    let a_is_greater = _mm512_cmpgt_epu32_mask(
                        _mm512_add_epi32(state[0], save_a),
                        _mm512_set1_epi32(target[0] as _),
                    );

                    if a_is_greater != 0 {
                        let success_lane_idx = _tzcnt_u32(a_is_greater as _) as usize;
                        let nonce_prefix = 10 + 16 * prefix_set_index + success_lane_idx as u64;

                        self.message[14] = cum0;
                        self.message[15] = cum1;
                        {
                            let message_bytes = decompose_blocks_mut(&mut self.message);
                            *message_bytes.get_unchecked_mut(
                                *SWAP_DWORD_BYTE_ORDER.get_unchecked(Self::DIGIT_IDX as usize),
                            ) = (nonce_prefix / 10) as u8 + b'0';
                            *message_bytes.get_unchecked_mut(
                                *SWAP_DWORD_BYTE_ORDER.get_unchecked(Self::DIGIT_IDX as usize + 1),
                            ) = (nonce_prefix % 10) as u8 + b'0';
                        }

                        // recompute the hash from the beginning
                        // this prevents the compiler from having to compute the final B-H registers alive in tight loops
                        // reverse the byte order
                        let mut final_sha_state = self.prefix_state.clone();
                        sha256::compress_block_reference(&mut final_sha_state, &self.message);
                        sha256::compress_block_reference(
                            &mut final_sha_state,
                            self.terminal_message_schedule[0..16].try_into().unwrap(),
                        );

                        let mut nonce_suffix = 0;
                        let mut key_copy = inner_key;
                        for _ in 0..7 {
                            nonce_suffix *= 10;
                            nonce_suffix += key_copy % 10;
                            key_copy /= 10;
                        }

                        // the nonce is the 8 digits in the message, plus the first two digits recomputed from the lane index
                        return Some((
                            nonce_prefix * 10u64.pow(7) + nonce_suffix as u64 + self.nonce_addend,
                            (final_sha_state[0] as u128) << 96
                                | (final_sha_state[1] as u128) << 64
                                | (final_sha_state[2] as u128) << 32
                                | (final_sha_state[3] as u128),
                        ));
                    }
                }
            }
        }

        None
    }
}

// this is a straight-forward implementation that is what I _think_ the official solution should have done with no dangerous or platform dependent optimizations
// it will use whatever sha2 crate uses (SHA-NI if available)
pub struct SingleBlockSolverNative {
    // the SHA-256 state A-H for all prefix bytes
    pub(crate) prefix_state: [u32; 8],

    // the message template for the final block
    pub(crate) message:
        sha2::digest::generic_array::GenericArray<u8, sha2::digest::generic_array::typenum::U64>,

    pub(crate) digit_index: usize,

    pub(crate) nonce_addend: u64,
}

impl Solver for SingleBlockSolverNative {
    type Ctx = ();

    fn new(_ctx: Self::Ctx, mut prefix: &[u8]) -> Option<Self> {
        // construct the message buffer
        let mut prefix_state = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut nonce_addend = 0u64;
        let mut complete_blocks_before = 0;

        // first consume all full blocks, this is shared so use scalar reference implementation
        while prefix.len() >= 64 {
            sha256::compress_block_reference(
                &mut prefix_state,
                &core::array::from_fn(|i| {
                    u32::from_be_bytes([
                        prefix[i * 4],
                        prefix[i * 4 + 1],
                        prefix[i * 4 + 2],
                        prefix[i * 4 + 3],
                    ])
                }),
            );
            prefix = &prefix[64..];
            complete_blocks_before += 1;
        }
        // if there is not enough room for 9 bytes of padding, '1's and then start a new block whenever possible
        // this avoids having to hash 2 blocks per iteration a naive solution would do
        if prefix.len() + 9 + 9 > 64 {
            let mut tmp_block = [0; 64];
            tmp_block[..prefix.len()].copy_from_slice(prefix);
            tmp_block[prefix.len()..].iter_mut().for_each(|b| {
                nonce_addend *= 10;
                nonce_addend += 1;
                *b = b'1';
            });
            nonce_addend = nonce_addend.checked_mul(1_000_000_000)?;
            complete_blocks_before += 1;
            prefix = &[];
            sha256::compress_block_reference(
                &mut prefix_state,
                &core::array::from_fn(|i| {
                    u32::from_be_bytes([
                        tmp_block[i * 4],
                        tmp_block[i * 4 + 1],
                        tmp_block[i * 4 + 2],
                        tmp_block[i * 4 + 3],
                    ])
                }),
            );
        }

        let mut message = sha2::digest::generic_array::GenericArray::default();
        let mut ptr = 0;
        message[..prefix.len()].copy_from_slice(prefix);
        ptr += prefix.len();
        let digit_index = ptr;

        // skip 9 zeroes, this is the part we will interpolate N into
        // the first 2 digits are used as the lane index (10 + (0..16)*(0..4), offset to avoid leading zeroes), this also keeps our proof plausible
        // the rest are randomly generated then broadcasted to all lanes
        // this gives us about 16e7 * 4 possible attempts, likely enough for any realistic deployment even on the highest difficulty
        // the fail rate would be pgeom(keySpace, 1/difficulty, lower=F) in R
        ptr += 9;

        // set up padding
        message[ptr] = 0x80;
        message[(64 - 8)..]
            .copy_from_slice(&((complete_blocks_before * 64 + ptr) as u64 * 8).to_be_bytes());

        Some(Self {
            prefix_state,
            message,
            digit_index,
            nonce_addend,
        })
    }

    fn solve(&mut self, target: [u32; 4]) -> Option<(u64, u128)> {
        // start from the blind-spot of the AVX-512 solution first
        for keyspace in [900_000_000..1_000_000_000, 100_000_000..900_000_000] {
            for key in keyspace {
                let mut key_copy = key;
                for i in (0..9).rev() {
                    self.message[self.digit_index + i] = (key_copy % 10) as u8 + b'0';
                    key_copy /= 10;
                }

                let mut state = self.prefix_state.clone();
                sha2::compress256(&mut state, &[self.message]);

                if state[0] > target[0] {
                    return Some((
                        key + self.nonce_addend,
                        (state[0] as u128) << 96
                            | (state[1] as u128) << 64
                            | (state[2] as u128) << 32
                            | (state[3] as u128),
                    ));
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn test_solve<S: Solver>() -> HashSet<usize>
    where
        <S as Solver>::Ctx: Default,
    {
        const SALT: &str = "z";

        let mut solved = HashSet::new();
        let mut cannot_solve = 0;
        for phrase_len in 0..64 {
            let mut concatenated_prefix = SALT.as_bytes().to_vec();
            let phrase_str = String::from_iter(std::iter::repeat('a').take(phrase_len));
            concatenated_prefix.extend_from_slice(&bincode::serialize(&phrase_str).unwrap());

            let config = pow_sha256::Config { salt: SALT.into() };
            const DIFFICULTY: u32 = 50_000;

            let solver = S::new(Default::default(), &concatenated_prefix);
            let Some(mut solver) = solver else {
                eprintln!(
                    "solver is None for phrase_len: {} (prefix: {})",
                    phrase_len,
                    concatenated_prefix.len()
                );
                cannot_solve += 1;
                continue;
            };
            solved.insert(phrase_len);
            let target_bytes = compute_target(DIFFICULTY).to_be_bytes();
            let target_u32s = core::array::from_fn(|i| {
                u32::from_be_bytes([
                    target_bytes[i * 4],
                    target_bytes[i * 4 + 1],
                    target_bytes[i * 4 + 2],
                    target_bytes[i * 4 + 3],
                ])
            });
            let (nonce, result) = solver.solve(target_u32s).expect("solver failed");

            /*
            let mut expected_message = concatenated_prefix.clone();
            let nonce_string = nonce.to_string();
            expected_message.extend_from_slice(nonce_string.as_bytes());
            let mut hasher = sha2::Sha256::default();
            hasher.update(&expected_message);
            let expected_hash = hasher.finalize();
            */

            let test_response = pow_sha256::PoWBuilder::default()
                .nonce(nonce)
                .result(result.to_string())
                .build()
                .unwrap();
            assert_eq!(
                config.calculate(&test_response, &phrase_str).unwrap(),
                result
            );

            assert!(config.is_valid_proof(&test_response, &phrase_str));
        }

        println!(
            "cannot_solve: {} out of 64 lengths using {} (success rate: {:.2}%)",
            cannot_solve,
            core::any::type_name::<S>(),
            (64 - cannot_solve) as f64 / 64.0 * 100.0
        );

        solved
    }

    #[test]
    fn test_solve_16way() {
        let solved_single_block = test_solve::<SingleBlockSolver16Way>();
        let solved_double_block = test_solve::<DoubleBlockSolver16Way>();
        let mut total_solved = solved_single_block
            .union(&solved_double_block)
            .collect::<Vec<_>>();
        total_solved.sort();
        for expect in 0..64 {
            assert!(
                total_solved.contains(&&expect),
                "{} not in {:?}",
                expect,
                total_solved
            );
        }
    }

    #[test]
    fn test_solve_sha2_crate() {
        test_solve::<SingleBlockSolverNative>();
    }
}
