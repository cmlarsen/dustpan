use super::{Ctx, Emit};
use crate::model::{Action, Category, Item, Verdict};
use crate::util::{run, which};
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

pub fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().unwrap_or(0.0);
    let mult = match unit.trim().to_ascii_uppercase().as_str() {
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => 1.0,
    };
    (n * mult) as u64
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Image {
    #[serde(rename = "ID")]
    pub id: String,
    pub repository: String,
    pub tag: String,
    pub size: String,
    #[serde(default)]
    pub created_since: String,
}

pub fn parse_images(text: &str) -> Vec<Image> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[derive(Debug, PartialEq)]
pub struct ImageGroup {
    pub id: String,
    pub labels: Vec<String>,
    pub refs: Vec<String>,
    pub size: String,
    pub created_since: String,
}

impl ImageGroup {
    pub fn name(&self) -> String {
        if self.labels.is_empty() {
            format!(
                "<none> image {}",
                self.id
                    .trim_start_matches("sha256:")
                    .get(..12)
                    .unwrap_or("")
            )
        } else {
            self.labels.join(", ")
        }
    }

    pub fn rmi_args(&self) -> Vec<&str> {
        let targets = if self.refs.is_empty() {
            vec![self.id.as_str()]
        } else {
            self.refs.iter().map(String::as_str).collect()
        };
        std::iter::once("rmi").chain(targets).collect()
    }
}

pub fn group_images(images: Vec<Image>) -> Vec<ImageGroup> {
    let mut out: Vec<ImageGroup> = Vec::new();
    for img in images {
        let label = (img.repository != "<none>").then(|| format!("{}:{}", img.repository, img.tag));
        let r = label.clone().filter(|_| img.tag != "<none>");
        match out.iter_mut().find(|g| g.id == img.id) {
            Some(g) => {
                g.labels.extend(label);
                g.refs.extend(r);
            }
            None => out.push(ImageGroup {
                id: img.id,
                labels: label.into_iter().collect(),
                refs: r.into_iter().collect(),
                size: img.size,
                created_since: img.created_since,
            }),
        }
    }
    out
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct DfRow {
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(default)]
    pub reclaimable: String,
    #[serde(default)]
    pub size: String,
}

pub fn build_cache_bytes(text: &str) -> Option<(u64, u64)> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<DfRow>(l).ok())
        .find(|r| r.kind == "Build Cache")
        .map(|r| {
            (
                parse_size(&r.size),
                parse_size(r.reclaimable.split(' ').next().unwrap_or("")),
            )
        })
}

pub fn classify_image(in_use: bool) -> (Verdict, Vec<String>) {
    if in_use {
        (Verdict::Active, vec!["a container uses this image".into()])
    } else {
        (
            Verdict::Review,
            vec![
                "no container uses this image".into(),
                "`docker rmi` removes it; pull or rebuild to get it back".into(),
            ],
        )
    }
}

fn used_image_ids() -> HashSet<String> {
    let Some(ids) = run("docker", &["ps", "-aq", "--no-trunc"], None, TIMEOUT) else {
        return HashSet::new();
    };
    let ids: Vec<&str> = ids.stdout.split_whitespace().collect();
    if ids.is_empty() {
        return HashSet::new();
    }
    let mut args = vec!["inspect", "--format", "{{.Image}}"];
    args.extend(ids);
    run("docker", &args, None, TIMEOUT)
        .map(|o| o.stdout.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

pub fn scan(ctx: &Ctx, emit: Emit) {
    if which("docker").is_none() {
        return;
    }
    if !run(
        "docker",
        &["info", "--format", "{{.ServerVersion}}"],
        None,
        Duration::from_secs(8),
    )
    .is_some_and(|o| o.ok)
    {
        return;
    }
    let vm_dir = ctx.home.join("Library/Containers/com.docker.docker");
    let used = used_image_ids();
    let images = run(
        "docker",
        &["images", "--no-trunc", "--format", "{{json .}}"],
        None,
        TIMEOUT,
    )
    .map(|o| parse_images(&o.stdout))
    .unwrap_or_default();
    for img in group_images(images) {
        let mut item = Item::new(Category::Docker, img.name(), &vm_dir);
        item.id = format!("docker_image:{}", img.id);
        item.bytes = parse_size(&img.size);
        item.reclaimable = item.bytes;
        let (verdict, mut reasons) = classify_image(used.contains(&img.id));
        if img.refs.len() > 1 {
            reasons.push(format!(
                "{} tags point at this image; removing it untags all of them",
                img.refs.len()
            ));
        }
        if !img.created_since.is_empty() {
            reasons.push(format!("built {}", img.created_since));
        }
        reasons.push("space is freed inside Docker's VM disk; volumes are never touched".into());
        item.verdict = verdict;
        item.reasons = reasons;
        item.action = Action::run("docker", &img.rmi_args(), None);
        emit(item);
    }
    if let Some((size, reclaimable)) = run(
        "docker",
        &["system", "df", "--format", "{{json .}}"],
        None,
        TIMEOUT,
    )
    .and_then(|o| build_cache_bytes(&o.stdout))
    {
        let mut item = Item::new(Category::Docker, "Docker build cache", &vm_dir);
        item.id = "docker_build_cache".into();
        item.bytes = size;
        item.reclaimable = reclaimable;
        item.verdict = Verdict::Safe;
        item.reasons =
            vec!["layer cache from `docker build`; the next build refills what it needs".into()];
        item.action = Action::run("docker", &["builder", "prune", "-f"], None);
        emit(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1.5GB"), 1_500_000_000);
        assert_eq!(parse_size("512MB"), 512_000_000);
        assert_eq!(parse_size("12.3kB"), 12_300);
        assert_eq!(parse_size("0B"), 0);
    }

    #[test]
    fn images_and_build_cache() {
        let images = "{\"Containers\":\"N/A\",\"CreatedSince\":\"3 weeks ago\",\"ID\":\"sha256:abc123abc123abc1\",\"Repository\":\"<none>\",\"Size\":\"1.2GB\",\"Tag\":\"<none>\"}\n{\"CreatedSince\":\"2 days ago\",\"ID\":\"sha256:def\",\"Repository\":\"postgres\",\"Size\":\"400MB\",\"Tag\":\"16\"}\n";
        let v = parse_images(images);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].repository, "postgres");
        let df = "{\"Active\":\"2\",\"Reclaimable\":\"3.1GB (100%)\",\"Size\":\"3.1GB\",\"TotalCount\":\"40\",\"Type\":\"Build Cache\"}\n{\"Type\":\"Images\",\"Size\":\"5GB\",\"Reclaimable\":\"1GB (20%)\"}\n";
        assert_eq!(build_cache_bytes(df), Some((3_100_000_000, 3_100_000_000)));
        assert_eq!(classify_image(true).0, Verdict::Active);
        assert_eq!(classify_image(false).0, Verdict::Review);
    }

    #[test]
    fn multi_tag_image_is_one_item_removed_by_every_tag() {
        let images = "{\"ID\":\"sha256:aaa\",\"Repository\":\"postgres\",\"Size\":\"400MB\",\"Tag\":\"16\"}\n{\"ID\":\"sha256:aaa\",\"Repository\":\"mirror/postgres\",\"Size\":\"400MB\",\"Tag\":\"latest\"}\n{\"ID\":\"sha256:bbb\",\"Repository\":\"<none>\",\"Size\":\"1GB\",\"Tag\":\"<none>\"}\n{\"ID\":\"sha256:ccc\",\"Repository\":\"app\",\"Size\":\"1GB\",\"Tag\":\"<none>\"}\n";
        let g = group_images(parse_images(images));
        assert_eq!(g.len(), 3);
        let ids: HashSet<&str> = g.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(g[0].name(), "postgres:16, mirror/postgres:latest");
        assert_eq!(
            g[0].rmi_args(),
            vec!["rmi", "postgres:16", "mirror/postgres:latest"]
        );
        assert_eq!(g[1].rmi_args(), vec!["rmi", "sha256:bbb"]);
        assert_eq!(g[2].name(), "app:<none>");
        assert_eq!(g[2].rmi_args(), vec!["rmi", "sha256:ccc"]);
    }
}
