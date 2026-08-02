//! Bulk metric history: `get_metric_history_bulk` and
//! `get_metric_history_bulk_interval` (plan T2.7), ported exactly from
//! `mlflow/store/tracking/sqlalchemy_store.py` (the SQL-based override) and the
//! handler caps in `mlflow/server/handlers.py`.
//!
//! ## `get_metric_history_bulk`
//!
//! Returns all logged values for `metric_key` across `run_ids` (≤100, enforced
//! by the handler), ordered by `(run_uuid, timestamp, step, value)` and capped
//! at `max_results` (≤25000). The ordering is a single global order across runs,
//! and the cap is a single global `LIMIT` — matching the Python `.limit()` on
//! the combined query. Run ids are first filtered to those accessible in the
//! workspace (mirrors `_filter_entity_ids(RUN)`), so out-of-workspace ids are
//! silently dropped rather than erroring.
//!
//! ## `get_metric_history_bulk_interval` (row sampling)
//!
//! Sampling is independent per run and bounded by `max_results`, even when many
//! values share one step. Rows are canonically ordered by
//! `(step, timestamp, value, is_nan)`, assigned with `NTILE(max_results)`, and
//! ranked within each bucket with `ROW_NUMBER`; the first row of every bucket is
//! kept. The global last row is then appended (or replaces the final sampled
//! row when already at the cap), preserving both boundaries. This is the
//! SQLAlchemy-store algorithm from upstream #24305.

use mlflow_error::MlflowError;

use super::dbutil::Val;
use super::entities::{Metric, MetricWithRunId};
use super::experiments::internal;
use super::TrackingStore;

/// Handler cap on run ids per bulk request (`MAX_RUN_IDS_PER_REQUEST` /
/// `MAX_RUNS_GET_METRIC_HISTORY_BULK`).
pub const MAX_RUNS_GET_METRIC_HISTORY_BULK: usize = 100;

/// Handler cap on sampled results per run for the interval API
/// (`MAX_RESULTS_PER_RUN`).
pub const MAX_RESULTS_PER_RUN: usize = 2500;

impl TrackingStore {
    /// `get_metric_history_bulk`: metrics for `metric_key` across `run_ids`,
    /// ordered by `(run_uuid, timestamp, step, value)`, globally capped at
    /// `max_results`. Run ids are filtered to the workspace first.
    pub async fn get_metric_history_bulk(
        &self,
        workspace: &str,
        run_ids: &[&str],
        metric_key: &str,
        max_results: usize,
    ) -> Result<Vec<MetricWithRunId>, MlflowError> {
        let accessible = self.filter_run_ids(workspace, run_ids).await?;
        if accessible.is_empty() {
            return Ok(Vec::new());
        }
        let dialect = self.db().dialect();
        let ph = |i| dialect.placeholder(i);

        let mut vals: Vec<Val> = vec![Val::Text(metric_key.to_string())];
        let placeholders: Vec<String> = accessible
            .iter()
            .enumerate()
            .map(|(i, rid)| {
                vals.push(Val::Text(rid.clone()));
                ph(i + 2)
            })
            .collect();

        let sql = format!(
            "SELECT run_uuid, \"key\", value, timestamp, step, is_nan FROM metrics \
             WHERE \"key\" = {} AND run_uuid IN ({}) \
             ORDER BY run_uuid, timestamp, step, value \
             LIMIT {}",
            ph(1),
            placeholders.join(", "),
            max_results,
        );

        self.db()
            .fetch_all(&sql, &vals, metric_with_run_id_from_row)
            .await
            .map_err(internal)
    }

    /// `get_metric_history_bulk_interval`: independently row-sampled metric
    /// history for each run. See the module docs for the exact SQL algorithm.
    pub async fn get_metric_history_bulk_interval(
        &self,
        workspace: &str,
        run_ids: &[&str],
        metric_key: &str,
        max_results: usize,
        start_step: Option<i64>,
        end_step: Option<i64>,
    ) -> Result<Vec<MetricWithRunId>, MlflowError> {
        if start_step.is_some() != end_step.is_some() {
            return Err(MlflowError::invalid_parameter_value(
                "Both start_step and end_step must be specified together, or neither may be specified.",
            ));
        }
        let max_results = max_results.max(1);

        for rid in run_ids {
            // Workspace access check per run (mirrors `_validate_run_accessible`).
            self.resolve_run_row(workspace, rid).await?;
        }

        let mut out = Vec::new();
        for rid in run_ids {
            out.extend(
                self.sample_metric_history_single_run(
                    rid,
                    metric_key,
                    max_results,
                    start_step,
                    end_step,
                )
                .await?,
            );
        }
        Ok(out)
    }

    async fn sample_metric_history_single_run(
        &self,
        run_id: &str,
        metric_key: &str,
        max_results: usize,
        start_step: Option<i64>,
        end_step: Option<i64>,
    ) -> Result<Vec<MetricWithRunId>, MlflowError> {
        let dialect = self.db().dialect();
        let ph = |i| dialect.placeholder(i);
        let mut vals: Vec<Val> = vec![
            Val::Text(run_id.to_string()),
            Val::Text(metric_key.to_string()),
        ];
        let mut filters = vec![
            format!("run_uuid = {}", ph(1)),
            format!("\"key\" = {}", ph(2)),
        ];
        if let (Some(start), Some(end)) = (start_step, end_step) {
            vals.push(Val::Int(start));
            vals.push(Val::Int(end));
            filters.push(format!("step >= {}", ph(3)));
            filters.push(format!("step <= {}", ph(4)));
        }
        let filters = filters.join(" AND ");
        let sql = format!(
            "WITH bucketed AS (\
                 SELECT run_uuid, \"key\", value, timestamp, step, is_nan, \
                        NTILE({max_results}) OVER (ORDER BY step, timestamp, value, is_nan) AS bucket \
                 FROM metrics WHERE {filters}\
             ), ranked AS (\
                 SELECT run_uuid, \"key\", value, timestamp, step, is_nan, \
                        ROW_NUMBER() OVER (\
                            PARTITION BY bucket ORDER BY step, timestamp, value, is_nan\
                        ) AS sample_rank \
                 FROM bucketed\
             ) \
             SELECT run_uuid, \"key\", value, timestamp, step, is_nan \
             FROM ranked WHERE sample_rank = 1 \
             ORDER BY step, timestamp, value, is_nan"
        );
        let mut rows = self
            .db()
            .fetch_all(&sql, &vals, sampled_metric_row_from_row)
            .await
            .map_err(internal)?;

        let last_sql = format!(
            "SELECT run_uuid, \"key\", value, timestamp, step, is_nan FROM metrics \
             WHERE {filters} ORDER BY step DESC, timestamp DESC, value DESC, is_nan DESC LIMIT 1"
        );
        let last = self
            .db()
            .fetch_optional(&last_sql, &vals, sampled_metric_row_from_row)
            .await
            .map_err(internal)?;
        if let Some(last) = last {
            if rows.last().is_none_or(|row| !row.same_canonical_row(&last)) {
                if rows.len() >= max_results {
                    *rows.last_mut().expect("sample is non-empty at the cap") = last;
                } else {
                    rows.push(last);
                }
            }
        }

        Ok(rows
            .into_iter()
            .map(SampledMetricRow::into_metric)
            .collect())
    }

    /// Filter `run_ids` to those whose experiment is in `workspace`
    /// (`_filter_entity_ids(RUN)`), preserving input order and dropping others.
    async fn filter_run_ids(
        &self,
        workspace: &str,
        run_ids: &[&str],
    ) -> Result<Vec<String>, MlflowError> {
        if run_ids.is_empty() {
            return Ok(Vec::new());
        }
        let dialect = self.db().dialect();
        let ph = |i| dialect.placeholder(i);
        let mut vals: Vec<Val> = Vec::with_capacity(run_ids.len() + 1);
        let placeholders: Vec<String> = run_ids
            .iter()
            .enumerate()
            .map(|(i, rid)| {
                vals.push(Val::Text(rid.to_string()));
                ph(i + 1)
            })
            .collect();
        vals.push(Val::Text(workspace.to_string()));
        let sql = format!(
            "SELECT r.run_uuid AS run_uuid FROM runs r \
             WHERE r.run_uuid IN ({}) AND r.experiment_id IN \
             (SELECT experiment_id FROM experiments WHERE workspace = {})",
            placeholders.join(", "),
            ph(run_ids.len() + 1)
        );
        let found: Vec<String> = self
            .db()
            .fetch_all(&sql, &vals, |r| r.get_string("run_uuid"))
            .await
            .map_err(internal)?;
        Ok(run_ids
            .iter()
            .filter(|rid| found.iter().any(|f| f == *rid))
            .map(|rid| rid.to_string())
            .collect())
    }
}

#[derive(Debug)]
struct SampledMetricRow {
    run_id: String,
    key: String,
    value: f64,
    timestamp: i64,
    step: i64,
    is_nan: bool,
}

impl SampledMetricRow {
    fn same_canonical_row(&self, other: &Self) -> bool {
        self.step == other.step
            && self.timestamp == other.timestamp
            && self.is_nan == other.is_nan
            && (self.is_nan || self.value == other.value)
    }

    fn into_metric(self) -> MetricWithRunId {
        MetricWithRunId {
            run_id: self.run_id,
            metric: Metric {
                key: self.key,
                value: if self.is_nan { f64::NAN } else { self.value },
                timestamp: self.timestamp,
                step: self.step,
            },
        }
    }
}

fn sampled_metric_row_from_row(
    r: &dyn super::dbutil::RowLike,
) -> Result<SampledMetricRow, sqlx::Error> {
    Ok(SampledMetricRow {
        run_id: r.get_string("run_uuid")?,
        key: r.get_string("key")?,
        value: r.get_f64("value")?,
        timestamp: r.get_opt_i64("timestamp")?.unwrap_or(0),
        step: r.get_i64("step")?,
        is_nan: r.get_bool("is_nan")?,
    })
}

fn metric_with_run_id_from_row(
    r: &dyn super::dbutil::RowLike,
) -> Result<MetricWithRunId, sqlx::Error> {
    let is_nan = r.get_bool("is_nan")?;
    let stored = r.get_f64("value")?;
    Ok(MetricWithRunId {
        run_id: r.get_string("run_uuid")?,
        metric: Metric {
            key: r.get_string("key")?,
            value: if is_nan { f64::NAN } else { stored },
            timestamp: r.get_opt_i64("timestamp")?.unwrap_or(0),
            step: r.get_i64("step")?,
        },
    })
}
