#![feature(drain_filter)]

use clap::{AppSettings, Clap};
use futures::stream::StreamExt;
use heim::process::{CpuUsage, Pid, Process, Status};
use heim::units::ratio;
use plotters::prelude::*;
use std::time::{Duration, Instant};
use system::{Features, System};

mod platform;
mod system;

fn main() {
    let mut opts: Opts = Opts::parse();
    let sys_category: Vec<String> = opts
        .category
        .drain_filter(|c| c.starts_with("sys_"))
        .collect();

    let (processes, cpu_info, cpu_frequency_max) = futures::executor::block_on(async {
        let mut features = Features::empty();

        if opts.category.contains(&"gpu".to_owned()) {
            features.insert(Features::GPU);
        }
        if sys_category.contains(&"sys_cpu_freq".to_string()) {
            features.insert(Features::CPU_FREQUENCY);
        }

        let mut system = System::new(features);

        let mut processes = opts.find_processes().await;
        let mut cpu_info: Vec<CpuInfo> = vec![];
        let mut cpu_frequency_max: f32 = 1000.0;

        let mut last_record_time = Instant::now();

        for i in 0..opts.times {
            let now = Instant::now();
            let since = now.saturating_duration_since(last_record_time);
            let delay = Duration::from_secs(opts.interval).saturating_sub(since);
            if !delay.is_zero() {
                futures_timer::Delay::new(delay).await;
                last_record_time = Instant::now();
            } else {
                last_record_time = now;
            }

            system.update();

            // Process
            'p: for process in processes.iter_mut() {
                let mut message = format!("{}({})", &process.name, process.process.pid(),);

                for (idx, c) in opts.category.iter().enumerate() {
                    match c.as_str() {
                        "cpu" => {
                            if let Some(cpu_percent) = process.poll_cpu_percent().await {
                                process.value_percents[idx].push(cpu_percent);

                                message.push_str(&format!(" / CPU {:.2}%", cpu_percent));
                            } else {
                                continue 'p;
                            }
                        }
                        "mem" => {
                            if let Some(mem_usage) = process.poll_mem_usage().await {
                                process.value_percents[idx].push(mem_usage);

                                message.push_str(&format!(" / MEM {:.2}M", mem_usage));
                            } else {
                                continue 'p;
                            }
                        }
                        "gpu" => {
                            if let Some(gpu_percent) = process.poll_gpu_percent(&mut system) {
                                process.value_percents[idx].push(gpu_percent);

                                message.push_str(&format!(" / GPU {:.2}%", gpu_percent));
                            } else {
                                continue 'p;
                            }
                        }
                        _ => unimplemented!(),
                    }
                }

                println!("{}", message);
            }

            // System
            for c in sys_category.iter() {
                match c.as_str() {
                    "sys_cpu_freq" => {
                        let cpu_frequency = system.cpu_frequency();

                        println!(
                            "CPU Frequency: [{}]",
                            cpu_frequency
                                .iter()
                                .map(|f| format!("{}MHz", f))
                                .collect::<Vec<String>>()
                                .join(", ")
                        );

                        if cpu_info.is_empty() {
                            cpu_info = cpu_frequency
                                .into_iter()
                                .map(|f| {
                                    cpu_frequency_max = cpu_frequency_max.max(f);
                                    CpuInfo { freq: vec![f] }
                                })
                                .collect();
                        } else {
                            for (sum, f) in cpu_info.iter_mut().zip(cpu_frequency.into_iter()) {
                                cpu_frequency_max = cpu_frequency_max.max(f);
                                sum.freq.push(f);
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }

            println!("================ {}/{}", i, opts.times);

            processes.drain_filter(|p| !p.valid);
        }

        (processes, cpu_info, cpu_frequency_max)
    });

    let output = if let Some(output) = opts.output {
        output
    } else {
        return;
    };

    let root = SVGBackend::new(
        output.as_str(),
        (
            1280,
            720 * opts.category.len() as u32 + sys_category.len() as u32,
        ),
    )
    .into_drawing_area();
    root.fill(&WHITE).unwrap();

    let areas = root.split_evenly((opts.category.len() + sys_category.len(), 1));

    // Draw process
    for idx_c in 0..opts.category.len() {
        let area = &areas[idx_c];
        let mut total = vec![];

        for process in processes.iter() {
            if total.len() < process.value_percents[idx_c].len() {
                total.extend_from_slice(
                    vec![0.0; process.value_percents[idx_c].len() - total.len()].as_slice(),
                );
            }
            for (a, b) in total.iter_mut().zip(process.value_percents[idx_c].iter()) {
                *a += *b;
            }
        }
        let mut max = 0.0f32;
        for &t in total.iter() {
            max = max.max(t);
        }

        let caption;
        let unit;

        match opts.category[idx_c].as_str() {
            "cpu" => {
                caption = "Process CPU Usage";
                unit = "%";
                max = max.max(100.0);
            }
            "mem" => {
                caption = "Process MEM Usage";
                unit = "M";
                max += 100.0;
            }
            "gpu" => {
                caption = "Process GPU Usage";
                unit = "%";
                max = max.max(100.0);
            }
            _ => unimplemented!(),
        };

        let mut chart = ChartBuilder::on(&area)
            .caption(caption, ("sans-serif", 30).into_font())
            .margin(10)
            .x_label_area_size(40)
            .y_label_area_size(50)
            .build_cartesian_2d(0..(opts.times - 1), 0f32..max)
            .unwrap();

        chart
            .configure_mesh()
            .y_label_formatter(&|y| format!("{}{}", y, unit))
            .draw()
            .unwrap();

        for (idx, process) in processes.iter().enumerate() {
            let color = Palette99::pick(idx).stroke_width(2).filled();
            chart
                .draw_series(LineSeries::new(
                    process.value_percents[idx_c]
                        .clone()
                        .into_iter()
                        .enumerate(),
                    color.clone(),
                ))
                .unwrap()
                .label(format!(
                    "{}({}) / AVG({:.2}{})",
                    &process.name,
                    process.process.pid(),
                    process.avg_percent(idx_c),
                    unit
                ))
                .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], color.clone()));
        }

        if processes.len() > 1 {
            // Total
            let avg: f32 = total.iter().map(|a| *a).sum::<f32>() / total.len() as f32;
            let color = Palette99::pick(processes.len()).stroke_width(2).filled();
            chart
                .draw_series(LineSeries::new(
                    total.into_iter().enumerate(),
                    color.clone(),
                ))
                .unwrap()
                .label(format!("Total / AVG({:.2}{})", avg, unit))
                .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], color.clone()));
        }

        chart
            .configure_series_labels()
            .background_style(&WHITE.mix(0.8))
            .border_style(&BLACK)
            .draw()
            .unwrap();
    }

    // Draw system
    let mut area_i = opts.category.len();

    for c in sys_category {
        let area = &areas[area_i];
        let mut chart;

        match c.as_str() {
            "sys_cpu_freq" => {
                chart = ChartBuilder::on(&area)
                    .caption("CPU Frequency", ("sans-serif", 30).into_font())
                    .margin(10)
                    .x_label_area_size(40)
                    .y_label_area_size(50)
                    .build_cartesian_2d(0..(opts.times - 1), 0f32..cpu_frequency_max)
                    .unwrap();

                chart
                    .configure_mesh()
                    .y_label_formatter(&|y| format!("{}MHz", y))
                    .draw()
                    .unwrap();

                for (idx, info) in cpu_info.iter().enumerate() {
                    let color = Palette99::pick(idx).stroke_width(2).filled();
                    chart
                        .draw_series(LineSeries::new(
                            info.freq.clone().into_iter().enumerate(),
                            color.clone(),
                        ))
                        .unwrap()
                        .label(format!("CPU{} / AVG({:.2}MHz)", idx, info.avg(),))
                        .legend(move |(x, y)| {
                            PathElement::new(vec![(x, y), (x + 20, y)], color.clone())
                        });
                }
            }
            _ => unreachable!(),
        }

        chart
            .configure_series_labels()
            .background_style(&WHITE.mix(0.8))
            .border_style(&BLACK)
            .draw()
            .unwrap();

        area_i += 1;
    }
}

struct ProcessInfo {
    process: Process,
    name: String,
    value_percents: Vec<Vec<f32>>,
    prev_cpu_usage: CpuUsage,
    valid: bool,
}

impl ProcessInfo {
    async fn new(process: Process, name: String, categories: &[String]) -> Self {
        let prev_cpu_usage = process.cpu_usage().await.unwrap();

        Self {
            process,
            name,
            value_percents: vec![vec![]; categories.len()],
            prev_cpu_usage,
            valid: true,
        }
    }

    async fn poll_cpu_percent(&mut self) -> Option<f32> {
        if let Ok(cpu_usage) = self.process.cpu_usage().await {
            let cpu_percent = (cpu_usage.clone()
                - std::mem::replace(&mut self.prev_cpu_usage, cpu_usage))
            .get::<ratio::percent>();

            Some(cpu_percent)
        } else {
            self.valid = false;
            None
        }
    }

    async fn poll_mem_usage(&mut self) -> Option<f32> {
        if let Ok(m) = self.process.memory().await {
            Some((m.rss().value as f64 / (1024.0 * 1024.0)) as _)
        } else {
            self.valid = false;
            None
        }
    }

    fn poll_gpu_percent(&mut self, system: &mut System) -> Option<f32> {
        let r = system.process_gpu_percent(self.process.pid());

        if r.is_none() {
            self.valid = false;
        }

        r
    }

    fn avg_percent(&self, idx: usize) -> f32 {
        if self.value_percents[idx].is_empty() {
            0.0
        } else {
            self.value_percents[idx].iter().sum::<f32>() / (self.value_percents[idx].len() as f32)
        }
    }
}

struct CpuInfo {
    freq: Vec<f32>,
}

impl CpuInfo {
    fn avg(&self) -> f32 {
        if self.freq.is_empty() {
            0.0
        } else {
            self.freq.iter().sum::<f32>() / (self.freq.len() as f32)
        }
    }
}

#[derive(Clap, Debug, Clone)]
#[clap(version = "0.1.10", author = "Xiaopeng Li <x.friday@outlook.com>")]
#[clap(setting = AppSettings::ColoredHelp)]
struct Opts {
    #[clap(short, long)]
    process: Vec<Pid>,
    #[clap(long)]
    name: Vec<String>,
    #[clap(short, long)]
    output: Option<String>,
    #[clap(short, long, default_value = "1")]
    interval: u64,
    #[clap(short, long, default_value = "30")]
    times: usize,
    #[clap(short, long, default_value = "cpu", possible_values = &["cpu", "mem", "gpu", "sys_cpu_freq"])]
    category: Vec<String>,
}

impl Opts {
    async fn find_processes(&self) -> Vec<ProcessInfo> {
        let mut processes = vec![];

        if self.name.is_empty() {
            for &pid in self.process.iter() {
                let p = heim::process::get(pid).await.unwrap();
                let name = p.name().await.unwrap();
                processes.push(ProcessInfo::new(p, name, self.category.as_slice()).await);
            }
            return processes;
        }

        let mut all = Box::pin(heim::process::processes().await.unwrap());

        while let Some(p) = all.next().await {
            let p = match p {
                Ok(p) => p,
                Err(_) => continue,
            };

            match p.status().await.unwrap() {
                Status::Zombie => continue,
                _ => {}
            }

            let name = p.name().await.unwrap();

            if self.process.contains(&p.pid()) {
                processes.push(ProcessInfo::new(p, name, self.category.as_slice()).await);
            } else {
                for n in self.name.iter() {
                    if name.contains(n) {
                        processes.push(ProcessInfo::new(p, name, self.category.as_slice()).await);
                        break;
                    }
                }
            }
        }

        processes
    }
}
