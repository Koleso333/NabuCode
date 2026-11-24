use chrono::{SecondsFormat, Utc};
use nabu_protocol::{Credits, Quota};

pub fn combine_credits<'a>(balances: impl Iterator<Item = &'a Credits>) -> Option<Credits> {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    balances.fold(None, |sum, balance| {
        let mut sum: Credits = sum.unwrap_or_default();
        sum.remaining += balance.remaining;
        sum.total += balance.total;
        if let Some(reset) = &balance.reset
            && reset > &now
            && sum.reset.as_ref().is_none_or(|next| reset < next)
        {
            sum.reset = Some(reset.clone());
        }
        Some(sum)
    })
}

/// Quota of several accounts per family, as NaTools sums it: the mean share left and the nearest reset.
pub fn combine<'a>(quotas: impl Iterator<Item = &'a Quota>) -> Vec<Quota> {
    let mut families: Vec<(String, Vec<&Quota>)> = Vec::new();
    for quota in quotas {
        match families
            .iter_mut()
            .find(|(family, _)| *family == quota.family)
        {
            Some((_, list)) => list.push(quota),
            None => families.push((quota.family.clone(), vec![quota])),
        }
    }
    families
        .into_iter()
        .map(|(family, list)| {
            let remaining = list.iter().map(|q| q.remaining).sum::<f64>() / list.len() as f64;
            let reset = list.iter().filter_map(|q| q.reset.clone()).min();
            Quota {
                family,
                remaining,
                reset,
            }
        })
        .collect()
}
