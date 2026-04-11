use std::sync::OnceLock;

struct G711Tables {
    alaw_to_linear: [i16; 256],
    linear_to_alaw: [u8; 65_536],
}

static TABLES: OnceLock<G711Tables> = OnceLock::new();

pub fn warmup_tables() {
    let _ = tables();
}

pub fn decode_alaw_frame(frame: &[u8]) -> Vec<i16> {
    let table = &tables().alaw_to_linear;
    frame.iter().map(|byte| table[*byte as usize]).collect()
}

pub fn encode_alaw_frame(frame: &[i16], volume: f32) -> Vec<u8> {
    let table = &tables().linear_to_alaw;
    frame
        .iter()
        .map(|sample| {
            let adjusted = adjust_volume(*sample, volume);
            table[adjusted as u16 as usize]
        })
        .collect()
}

pub fn adjust_volume(sample: i16, volume: f32) -> i16 {
    let scaled = (sample as f32 * volume).clamp(i16::MIN as f32, i16::MAX as f32);
    scaled as i16
}

fn tables() -> &'static G711Tables {
    TABLES.get_or_init(|| {
        let mut alaw_to_linear = [0_i16; 256];
        for (index, slot) in alaw_to_linear.iter_mut().enumerate() {
            *slot = raw_alaw_to_linear(index as u8);
        }

        let mut linear_to_alaw = [0_u8; 65_536];
        for (index, slot) in linear_to_alaw.iter_mut().enumerate() {
            *slot = raw_linear_to_alaw(index as i16);
        }

        G711Tables {
            alaw_to_linear,
            linear_to_alaw,
        }
    })
}

fn raw_alaw_to_linear(code: u8) -> i16 {
    let code = code ^ 0x55;
    let exponent = ((code & 0x70) >> 4) as i16;
    let mut mantissa = (code & 0x0f) as i16;

    if exponent > 0 {
        mantissa += 16;
    }
    mantissa = (mantissa << 4) + 0x08;
    if exponent > 1 {
        mantissa <<= exponent - 1;
    }

    if code & 0x80 != 0 {
        mantissa
    } else {
        -mantissa
    }
}

fn raw_linear_to_alaw(sample: i16) -> u8 {
    let (sign, ix) = if sample < 0 {
        (0x80_u8, ((sample ^ -1_i16) as u16 >> 4) as i16)
    } else {
        (0x00_u8, (sample as u16 >> 4) as i16)
    };

    let mut value = ix;
    if value > 15 {
        let mut exponent = 1_u8;
        while value > 31 {
            value >>= 1;
            exponent += 1;
        }
        value -= 16;
        value += (exponent << 4) as i16;
    }

    let out = if sign == 0 { value | 0x80 } else { value };
    (out as u8) ^ 0x55
}
