//! Values as SQL Server shows them. tiberius decodes the protocol into typed
//! values; they become text here without passing through floating point
//! (except `float` and `real`, which are floating point) and without time
//! zone conversions.

use jykre_core::{Cell, DriverError, DriverResult, ErrorCode};
use tiberius::{ColumnData, ColumnType};

/// The name a column's type is shown with. The protocol does not always
/// carry the declared size (`intn` covers every integer), so sized types
/// are named by their family.
pub fn type_name(column_type: ColumnType) -> &'static str {
    match column_type {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 | ColumnType::Intn => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money => "money",
        ColumnType::Money4 => "smallmoney",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigBinary => "binary",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

/// One value of a column of `column_type`.
pub fn cell(column_type: ColumnType, data: &ColumnData<'static>) -> DriverResult<Cell> {
    let text = |value: String| Ok(Cell::Text(value));
    match data {
        ColumnData::U8(Some(value)) => text(value.to_string()),
        ColumnData::I16(Some(value)) => text(value.to_string()),
        ColumnData::I32(Some(value)) => text(value.to_string()),
        ColumnData::I64(Some(value)) => text(value.to_string()),
        ColumnData::F32(Some(value)) => text(value.to_string()),
        // tiberius decodes money as a float; its exact value is recovered
        // while that is still possible.
        ColumnData::F64(Some(value))
            if matches!(column_type, ColumnType::Money | ColumnType::Money4) =>
        {
            money(*value).map(Cell::Text)
        }
        ColumnData::F64(Some(value)) => text(value.to_string()),
        ColumnData::Bit(Some(value)) => text(if *value { "1" } else { "0" }.into()),
        ColumnData::String(Some(value)) => text(value.to_string()),
        ColumnData::Guid(Some(value)) => text(value.to_string().to_ascii_uppercase()),
        ColumnData::Binary(Some(value)) => Ok(Cell::Binary(value.to_vec())),
        ColumnData::Numeric(Some(value)) => text(scaled(value.value(), value.scale())),
        ColumnData::Xml(Some(value)) => text(value.to_string()),
        ColumnData::DateTime(Some(value)) => {
            text(datetime(value.days(), value.seconds_fragments()))
        }
        ColumnData::SmallDateTime(Some(value)) => {
            text(smalldatetime(value.days(), value.seconds_fragments()))
        }
        ColumnData::Date(Some(value)) => text(date(value.days())),
        ColumnData::Time(Some(value)) => text(time(value.increments(), value.scale())),
        ColumnData::DateTime2(Some(value)) => text(datetime2(
            value.date().days(),
            value.time().increments(),
            value.time().scale(),
        )),
        ColumnData::DateTimeOffset(Some(value)) => {
            let local = value.datetime2();
            text(datetimeoffset(
                local.date().days(),
                local.time().increments(),
                local.time().scale(),
                value.offset(),
            ))
        }
        _ => Ok(Cell::Null),
    }
}

/// `value / 10^scale`, written exactly.
pub fn scaled(value: i128, scale: u8) -> String {
    let digits = value.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    format!("{sign}{whole}.{fraction}")
}

/// The largest money value, scaled by 10⁴, whose float tiberius produced
/// still identifies it: below 2⁵² a division by 10⁴ rounded to the nearest
/// float is less than half a unit of the scaled value away.
const EXACT_MONEY: u64 = (1 << 52) - 2;

/// The exact text of a money value tiberius decoded as `value`, the float
/// nearest to the scaled integer divided by 10⁴.
pub fn money(value: f64) -> DriverResult<String> {
    scaled_money(value)
        .map(|units| scaled(i128::from(units), 4))
        .ok_or_else(|| {
            DriverError::new(
                ErrorCode::Protocol,
                "A money value is too large to be read exactly; cast it to decimal(19,4)",
            )
        })
}

/// The integer nearest to `value × 10⁴`, computed without rounding, while it
/// is below [`EXACT_MONEY`].
fn scaled_money(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let bits = value.to_bits();
    let negative = bits >> 63 == 1;
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1 << 52) - 1);
    // value = mantissa × 2^shift, exactly.
    let (mantissa, shift) = if exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), exponent - 1075)
    };
    let product = u128::from(mantissa) * 10_000;
    let units = if shift >= 0 {
        if shift > 20 {
            return None;
        }
        product << shift
    } else {
        let right = shift.unsigned_abs();
        if right >= 100 {
            0
        } else {
            (product + (1u128 << (right - 1))) >> right
        }
    };
    if units > u128::from(EXACT_MONEY) {
        return None;
    }
    let units = i64::try_from(units).ok()?;
    Some(if negative { -units } else { units })
}

/// Days since 1970-01-01 as a proleptic Gregorian date (H. Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// 1900-01-01, where `datetime` and `smalldatetime` count days from.
const FROM_1900: i64 = -25_567;
/// 0001-01-01, where `date` and `datetime2` count days from.
const FROM_0001: i64 = -719_162;

fn day_text(days_since_1970: i64) -> String {
    let (year, month, day) = civil(days_since_1970);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `seconds` since midnight with a `scale`-digit fraction.
fn clock(seconds: u64, fraction: u64, scale: u8) -> String {
    let text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if scale == 0 {
        text
    } else {
        format!("{text}.{fraction:0width$}", width = usize::from(scale))
    }
}

pub fn date(days: u32) -> String {
    day_text(FROM_0001 + i64::from(days))
}

pub fn time(increments: u64, scale: u8) -> String {
    let unit = 10u64.pow(u32::from(scale));
    clock(increments / unit, increments % unit, scale)
}

/// `datetime` counts 1/300 seconds; SQL Server shows them rounded to
/// milliseconds (.000, .003, .007).
pub fn datetime(days: i32, fragments: u32) -> String {
    let millis = (u64::from(fragments) * 10 + 1) / 3;
    format!(
        "{} {}",
        day_text(FROM_1900 + i64::from(days)),
        clock(millis / 1000, millis % 1000, 3)
    )
}

/// `smalldatetime` counts minutes.
pub fn smalldatetime(days: u16, minutes: u16) -> String {
    format!(
        "{} {}",
        day_text(FROM_1900 + i64::from(days)),
        clock(u64::from(minutes) * 60, 0, 0)
    )
}

pub fn datetime2(days: u32, increments: u64, scale: u8) -> String {
    format!("{} {}", date(days), time(increments, scale))
}

/// The protocol carries the UTC time and the offset; the value is shown as
/// it was written: the local time with its offset.
pub fn datetimeoffset(days: u32, increments: u64, scale: u8, offset: i16) -> String {
    let unit = i128::from(10u64.pow(u32::from(scale)));
    let per_day = 86_400 * unit;
    let utc = i128::from(days) * per_day + i128::from(increments);
    let local = utc + i128::from(offset) * 60 * unit;
    let day = local.div_euclid(per_day);
    let within = local.rem_euclid(per_day);
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs();
    format!(
        "{} {} {sign}{:02}:{:02}",
        day_text(FROM_0001 + day as i64),
        time(within as u64, scale),
        minutes / 60,
        minutes % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_keep_every_digit() {
        assert_eq!(scaled(12_345, 2), "123.45");
        assert_eq!(scaled(-5, 3), "-0.005");
        assert_eq!(scaled(0, 4), "0.0000");
        assert_eq!(scaled(42, 0), "42");
        assert_eq!(
            scaled(99_999_999_999_999_999_999_999_999_999_999_999_999, 10),
            "9999999999999999999999999999.9999999999"
        );
    }

    #[test]
    fn money_is_exact_while_the_float_identifies_it() {
        for units in [
            0i64,
            1,
            -1,
            12_345,
            1_000_000_000_000_001,
            -2_147_483_648,
            2_147_483_647,
            EXACT_MONEY as i64,
            -(EXACT_MONEY as i64),
        ] {
            // What tiberius computes from the wire value.
            let float = units as f64 / 1e4;
            assert_eq!(
                money(float).unwrap(),
                scaled(i128::from(units), 4),
                "{units}"
            );
        }
        assert_eq!(money(0.1234).unwrap(), "0.1234");
        assert_eq!(money(-214_748.364_8).unwrap(), "-214748.3648");
        // Beyond the bound the last digits cannot be told apart.
        let max = 9_223_372_036_854_775_807i64 as f64 / 1e4;
        assert_eq!(money(max).unwrap_err().code, ErrorCode::Protocol);
        assert!(money(f64::NAN).is_err());
    }

    #[test]
    fn dates_and_times_are_written_as_stored() {
        assert_eq!(date(0), "0001-01-01");
        assert_eq!(date(3_652_058), "9999-12-31");
        assert_eq!(date(738_578), "2023-02-28");
        assert_eq!(time(0, 0), "00:00:00");
        assert_eq!(time(863_999_999_999, 7), "23:59:59.9999999");
        assert_eq!(time(45_296_123, 3), "12:34:56.123");
        assert_eq!(datetime(0, 0), "1900-01-01 00:00:00.000");
        assert_eq!(datetime(-53_690, 0), "1753-01-01 00:00:00.000");
        assert_eq!(datetime(0, 1), "1900-01-01 00:00:00.003");
        assert_eq!(datetime(0, 2), "1900-01-01 00:00:00.007");
        assert_eq!(datetime(2_958_463, 25_919_999), "9999-12-31 23:59:59.997");
        assert_eq!(smalldatetime(0, 754), "1900-01-01 12:34:00");
        assert_eq!(
            datetime2(738_578, 452_961_234_567, 7),
            "2023-02-28 12:34:56.1234567"
        );
        // 2023-02-28 23:30 UTC is 2023-03-01 02:30 at +03:00.
        assert_eq!(
            datetimeoffset(738_578, 84_600, 0, 180),
            "2023-03-01 02:30:00 +03:00"
        );
        assert_eq!(
            datetimeoffset(738_578, 3_600_000, 3, -90),
            "2023-02-27 23:30:00.000 -01:30"
        );
    }
}
