use anyhow::{bail, Result};
use glob::glob;
use reqwest::blocking as reqwest;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

static ALLOWED_PREFIXES: &[&str] = &[
    "https://github.com/kov/iocost-benchmarks/files/",
    "https://iocost-submit.s3.eu-west-1.amazonaws.com/",
];

fn is_url_whitelisted(link: &str) -> bool {
    for prefix in ALLOWED_PREFIXES {
        if link.starts_with(prefix) {
            return true;
        }
    }

    false
}

fn get_urls(context: &json::JsonValue) -> Result<Vec<String>> {
    let issue = &context["event"]["issue"];
    if issue["locked"].as_bool().unwrap() || issue["state"] != "open" {
        println!("Issue is either locked or not in the open state, doing nothing...");
        std::process::exit(0);
    }

    // created is always for comments, opened is always for issues.
    let body = match context["event"]["action"].as_str().unwrap() {
        "created" => context["event"]["comment"]["body"].as_str(),
        "opened" => issue["body"].as_str(),
        "edited" => {
            if context["event_name"] == "issue_comment" {
                context["event"]["comment"]["body"].as_str()
            } else {
                issue["body"].as_str()
            }
        }
        _ => bail!(
            "Called for event we do not handle: {} / {}",
            context["event_name"],
            context["event"]["action"]
        ),
    }
    .expect("Could not obtain the contents of the issue or comment");

    let mut urls = vec![];
    for link in linkify::LinkFinder::new().links(body) {
        let link = link.as_str();

        if is_url_whitelisted(link) {
            println!("URL found: {}", link);
            urls.push(link.to_string());
        } else {
            println!(
                "URL ignored due to not having a whitelisted prefix: {}",
                link
            );
        }
    }

    Ok(urls)
}

fn download_url(url: &str) -> Result<String> {
    println!("download_url: {}", url);
    let response = reqwest::get(url)?;

    let contents = response.bytes()?;

    // Use md5sum of the data as filename, we only care about exact duplicates.
    //let tmpdir = tempfile::Builder::new().prefix("iocost-benchmark").tempdir()?;
    let filename = format!("result-{:x}.json.gz", md5::compute(&contents));

    let path = PathBuf::from(&filename); // tmpdir.path().join(&filename);

    let mut file = fs::File::create(&path)?;
    file.write_all(&contents)?;

    Ok(path.to_string_lossy().to_string())
}

fn get_normalized_model_name(filename: &str) -> Result<String> {
    let output = std::process::Command::new("./resctl-demo/target/release/resctl-bench")
        .args(&["--result", filename, "info"])
        .output()?;

    if !output.stderr.is_empty() {
        panic!("{}", String::from_utf8(output.stderr)?);
    }

    let output = String::from_utf8(output.stdout)?;
    Ok(output
        .split_once('\n')
        .unwrap()
        .0
        .split_once(": ")
        .unwrap()
        .1
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join("_"))
}

fn merge_results_in_dir(path: &Path) -> Result<PathBuf> {
    let results = glob(&format!(
        "{}/result-*.json.gz",
        path.to_string_lossy().to_string()
    ))
    .unwrap()
    .into_iter()
    .flatten()
    .map(|p| p.to_string_lossy().to_string());

    let merged_path = path.join("merged-results.json.gz");
    let mut arguments = vec![
        "--result".to_string(),
        merged_path.to_string_lossy().to_string(),
        "merge".to_string(),
    ];
    arguments.extend(results);

    let output = std::process::Command::new("./resctl-demo/target/release/resctl-bench")
        .args(arguments.as_slice())
        .output()?;

    if !output.stderr.is_empty() {
        panic!("{}", String::from_utf8(output.stderr)?);
    }

    let output = String::from_utf8(output.stdout)?;
    println!("{}", output);
    Ok(merged_path)
}

fn main() -> Result<()> {
    let token = std::env::var("GITHUB_TOKEN")?;

    /*octocrab::initialise(octocrab::Octocrab::builder().personal_token(token))?;*/
    let context = json::parse(&std::env::var("GITHUB_CONTEXT")?)?;

    let git_repo = git2::Repository::open(".")?;
    let mut index = git_repo.index()?;

    let mut directories_to_merge = vec![];

    // Download and validate all provided URLs.
    let urls = get_urls(&context)?;
    for url in urls {
        let filename = download_url(&url)?;
        let model_name = get_normalized_model_name(&filename)?;

        let model_directory = PathBuf::from(format!("database/{}", model_name));
        fs::create_dir(&model_directory).ok();

        let database_file = model_directory.join(&filename);
        fs::rename(&filename, &database_file)?;

        index.add_path(&database_file)?;

        directories_to_merge.push(model_directory);
    }

    // Call rectl-bench to merge all files for the directories with new files.
    for dir in &directories_to_merge {
        let merged_path = merge_results_in_dir(dir.as_path())?;
        index.add_path(&merged_path)?;
    }

    // Commit the new and changed files.
    let sig = git2::Signature::now("iocost bot", "gustavo.noronha@collabora.com")?;

    let parent_commit = git_repo.head()?.peel_to_commit()?;

    let oid = index.write_tree()?;
    let tree = git_repo.find_tree(oid)?;

    let commit_mesasge = format!(
        "Updated {}",
        directories_to_merge
            .iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect::<Vec<String>>()
            .join(", ")
    );

    let commit = git_repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        &commit_mesasge,
        &tree,
        &[&parent_commit],
    )?;

    let branch_name = format!("iocost-bot/{}", context["event"]["issue"]["id"]);
    git_repo.branch(&branch_name, &git_repo.find_commit(commit)?, true)?;

    // Push to a branch and send a PR.
    let mut callbacks = git2::RemoteCallbacks::new();
    callbacks.credentials(|_url, _username_from_url, _allowed_types| {
        git2::Cred::userpass_plaintext("iocost-bot", &token)
    });
    callbacks.push_update_reference(|name, status| {
        if let Some(e) = status {
            panic!("Error: {e}");
        }
        Ok(())
    });

    println!("Pushing to branch {branch_name}...");
    let refspec = format!("+HEAD:refs/heads/{branch_name}");
    let mut remote = git_repo.find_remote("httpsorigin")?;
    remote.push(
        &[&refspec],
        Some(git2::PushOptions::new().remote_callbacks(callbacks)),
    )?;

    /*
        let issue = octocrab::instance()
            .issues("iocost-benchmark", "iocost-benchmarks")
            .create(title)
            .body(body)
            .send()
            .await?;
    */
    Ok(())
}
