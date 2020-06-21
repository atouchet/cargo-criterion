use crate::connection::Throughput;
use crate::estimate::Estimates;
use crate::report::{BenchmarkId, MeasurementData};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use linked_hash_map::LinkedHashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug)]
pub struct Benchmark {
    latest_stats: Option<SavedStatistics>,
    previous_stats: Option<SavedStatistics>,
    target: Option<String>,
}
impl Default for Benchmark {
    fn default() -> Self {
        Benchmark {
            latest_stats: None,
            previous_stats: None,
            target: None,
        }
    }
}
impl Benchmark {
    fn add_stats(&mut self, stats: SavedStatistics) {
        self.previous_stats = self.latest_stats.take();
        self.latest_stats = Some(stats);
    }
}

#[derive(Debug)]
pub struct BenchmarkGroup {
    benchmarks: LinkedHashMap<BenchmarkId, Benchmark>,
    target: Option<String>,
}
impl Default for BenchmarkGroup {
    fn default() -> Self {
        BenchmarkGroup {
            benchmarks: LinkedHashMap::new(),
            target: None,
        }
    }
}

#[derive(Debug)]
pub struct Model {
    // Path to output directory
    data_directory: PathBuf,
    // Track all of the unique benchmark titles and directories we've seen, so we can uniquify them.
    all_titles: HashSet<String>,
    all_directories: HashSet<PathBuf>,

    groups: LinkedHashMap<String, BenchmarkGroup>,
}
impl Model {
    pub fn load(criterion_home: PathBuf, timeline: PathBuf) -> Model {
        let mut model = Model {
            data_directory: path!(criterion_home, "data", timeline),
            all_titles: HashSet::new(),
            all_directories: HashSet::new(),
            groups: LinkedHashMap::new(),
        };

        for entry in WalkDir::new(&model.data_directory)
            .into_iter()
            // Ignore errors.
            .filter_map(::std::result::Result::ok)
            .filter(|entry| entry.file_name() == OsStr::new("benchmark.cbor"))
        {
            match model.load_stored_benchmark(entry.path()) {
                Err(e) => error!("Encountered error while loading stored data: {}", e),
                _ => (),
            }
        }

        model
    }

    fn load_stored_benchmark(&mut self, benchmark_path: &Path) -> Result<()> {
        if !benchmark_path.is_file() {
            return Ok(());
        }
        let mut benchmark_file = File::open(&benchmark_path)
            .with_context(|| format!("Failed to open benchmark file {:?}", benchmark_path))?;
        let benchmark_record: BenchmarkRecord = serde_cbor::from_reader(&mut benchmark_file)
            .with_context(|| format!("Failed to read benchmark file {:?}", benchmark_path))?;

        let measurement_path = benchmark_path.with_file_name(benchmark_record.latest_record);
        if !measurement_path.is_file() {
            return Ok(());
        }
        let mut measurement_file = File::open(&measurement_path)
            .with_context(|| format!("Failed to open measurement file {:?}", measurement_path))?;
        let saved_stats: SavedStatistics = serde_cbor::from_reader(&mut measurement_file)
            .with_context(|| format!("Failed to read benchmark file {:?}", measurement_path))?;

        self.groups
            .entry(benchmark_record.id.group_id.clone())
            .or_insert_with(|| Default::default())
            .benchmarks
            .entry(benchmark_record.id.into())
            .or_insert_with(|| Default::default())
            .latest_stats = Some(saved_stats);
        Ok(())
    }

    pub fn add_benchmark_id(&mut self, target: &str, id: &mut BenchmarkId) {
        id.ensure_directory_name_unique(&self.all_directories);
        self.all_directories
            .insert(id.as_directory_name().to_owned());

        id.ensure_title_unique(&self.all_titles);
        self.all_titles.insert(id.as_title().to_owned());

        let group = self
            .groups
            .entry(id.group_id.clone())
            .or_insert_with(|| Default::default());

        let mut benchmark = group.benchmarks.remove(id).unwrap_or_default();

        if let Some(target) = &benchmark.target {
            warn!("Benchmark ID {} encountered multiple times. Benchmark IDs must be unique. First seen in the benchmark target '{}'", id.as_title(), target);
        } else {
            benchmark.target = Some(target.to_owned());
        }

        // Remove and re-insert to move the benchmark to the end of its list.
        group.benchmarks.insert(id.clone(), benchmark);
    }

    pub fn benchmark_complete(
        &mut self,
        id: &BenchmarkId,
        analysis_results: &MeasurementData,
    ) -> Result<()> {
        let dir = path!(&self.data_directory, id.as_directory_name());

        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create directory {:?}", dir))?;

        let measurement_name = chrono::Local::now()
            .format("measurement_%y%m%d%H%M%S.cbor")
            .to_string();

        let saved_stats = SavedStatistics {
            datetime: chrono::Utc::now(),
            iterations: analysis_results.iter_counts().to_vec(),
            values: analysis_results.sample_times().to_vec(),
            avg_values: analysis_results.avg_times.to_vec(),
            estimates: analysis_results.absolute_estimates.clone(),
            throughput: analysis_results.throughput.clone(),
        };

        let measurement_path = dir.join(&measurement_name);
        let mut measurement_file = File::create(&measurement_path)
            .with_context(|| format!("Failed to create measurement file {:?}", measurement_path))?;
        serde_cbor::to_writer(&mut measurement_file, &saved_stats).with_context(|| {
            format!("Failed to save measurements to file {:?}", measurement_path)
        })?;

        let record = BenchmarkRecord {
            id: id.into(),
            latest_record: PathBuf::from(&measurement_name),
        };

        let benchmark_path = dir.join("benchmark.cbor");
        let mut benchmark_file = File::create(&benchmark_path)
            .with_context(|| format!("Failed to create benchmark file {:?}", benchmark_path))?;
        serde_cbor::to_writer(&mut benchmark_file, &record)
            .with_context(|| format!("Failed to save benchmark file {:?}", benchmark_path))?;

        let benchmark = self
            .groups
            .get_mut(&id.group_id)
            .and_then(|g| g.benchmarks.get_mut(&id))
            .unwrap();
        benchmark.add_stats(saved_stats);
        Ok(())
    }

    pub fn get_last_sample(&self, id: &BenchmarkId) -> Option<&SavedStatistics> {
        self.groups
            .get(&id.group_id)
            .and_then(|g| g.benchmarks.get(id))
            .and_then(|b| b.latest_stats.as_ref())
    }

    pub fn check_benchmark_group(&self, current_target: &str, group: &str) {
        if let Some(benchmark_group) = self.groups.get(group) {
            if let Some(target) = &benchmark_group.target {
                if target != current_target {
                    warn!("Benchmark group {} encountered again. Benchmark group IDs must be unique. First seen in the benchmark target '{}'", group, target);
                }
            }
        }
    }

    pub fn add_benchmark_group(&mut self, target: &str, group_name: String) {
        // Remove and reinsert so that the group will be at the end of the map.
        let mut group = self.groups.remove(&group_name).unwrap_or_default();
        group.target = Some(target.to_owned());
        self.groups.insert(group_name, group);
    }
}

// These structs are saved to disk and may be read by future versions of cargo-criterion, so
// backwards compatibility is important.

#[derive(Debug, Deserialize, Serialize)]
pub struct SavedBenchmarkId {
    group_id: String,
    function_id: Option<String>,
    value_str: Option<String>,
    throughput: Option<Throughput>,
}
impl From<BenchmarkId> for SavedBenchmarkId {
    fn from(other: BenchmarkId) -> Self {
        SavedBenchmarkId {
            group_id: other.group_id,
            function_id: other.function_id,
            value_str: other.value_str,
            throughput: other.throughput,
        }
    }
}
impl From<&BenchmarkId> for SavedBenchmarkId {
    fn from(other: &BenchmarkId) -> Self {
        other.clone().into()
    }
}
impl From<SavedBenchmarkId> for BenchmarkId {
    fn from(other: SavedBenchmarkId) -> Self {
        BenchmarkId::new(
            other.group_id,
            other.function_id,
            other.value_str,
            other.throughput,
        )
    }
}
impl From<&SavedBenchmarkId> for BenchmarkId {
    fn from(other: &SavedBenchmarkId) -> Self {
        other.clone().into()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BenchmarkRecord {
    id: SavedBenchmarkId,
    latest_record: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SavedStatistics {
    pub datetime: DateTime<Utc>,
    pub iterations: Vec<f64>,
    pub values: Vec<f64>,
    pub avg_values: Vec<f64>,
    pub estimates: Estimates,
    pub throughput: Option<Throughput>,
}