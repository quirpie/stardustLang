//! Tipo `Date` (fecha civil del calendario gregoriano proléptico), sin dependencias.
//!
//! Internamente todo se apoya en la conversión fecha <-> número de días desde
//! `1970-01-01`, usando el algoritmo de Howard Hinnant. Eso hace que la
//! aritmética de fechas (sumar días, diferencia entre fechas) y la comparación
//! sean triviales y correctas respecto a años bisiestos.

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub struct Date {
    pub y: i64,
    pub m: i64,
    pub d: i64,
}

/// Días desde 1970-01-01 para una fecha civil (algoritmo de Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Fecha civil (y, m, d) a partir de días desde 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Date {
    /// Parsea `YYYY-MM-DD`. Devuelve `None` si el formato o la fecha no son válidos
    /// (p. ej. `2026-02-30`), validando por ida y vuelta contra el calendario.
    pub fn parse(s: &str) -> Option<Date> {
        let parts: Vec<&str> = s.trim().split('-').collect();
        if parts.len() != 3 {
            return None;
        }
        let y = parts[0].parse::<i64>().ok()?;
        let m = parts[1].parse::<i64>().ok()?;
        let d = parts[2].parse::<i64>().ok()?;
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        let date = Date { y, m, d };
        // Validación: reconstruir desde el nº de días debe dar la misma fecha.
        if civil_from_days(date.to_days()) != (y, m, d) {
            return None;
        }
        Some(date)
    }

    pub fn to_days(&self) -> i64 {
        days_from_civil(self.y, self.m, self.d)
    }

    pub fn from_days(z: i64) -> Date {
        let (y, m, d) = civil_from_days(z);
        Date { y, m, d }
    }

    /// Devuelve una nueva fecha desplazada `n` días (n puede ser negativo).
    pub fn add_days(&self, n: i64) -> Date {
        Date::from_days(self.to_days() + n)
    }

    /// Días entre `self` y `other` (`self - other`).
    pub fn days_between(&self, other: &Date) -> i64 {
        self.to_days() - other.to_days()
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.y, self.m, self.d)
    }
}
