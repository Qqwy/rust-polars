use awscreds::Credentials;
use cloud::AmazonS3ConfigKey as Key;
use polars::prelude::*;

// Login to your aws account and then copy the ../datasets/foods1.parquet file to your own bucket.
// Adjust the link below.
const TEST_S3: &str = "s3://lov2test/polars/datasets/*.parquet";

fn main() -> PolarsResult<()> {
    // let cred = Credentials::default().unwrap();

    // Propagate the credentials and other cloud options.
    // let mut args = ScanArgsParquet::default();
    // let cloud_options = cloud::CloudOptions::default().with_aws([
    //     (Key::AccessKeyId, &cred.access_key.unwrap()),
    //     (Key::SecretAccessKey, &cred.secret_key.unwrap()),
    //     (Key::Region, &"us-west-2".into()),
    // ]);
    // args.cloud_options = Some(cloud_options);

    let df =
        df!(
            "foo" => &[1, 2, 3],
            "bar" => &[None, Some("bak"), Some("baz")],
        )
        .unwrap();

    let uri = "file///tmp/polars_write_example.parquet".to_string();
    let df = df.lazy().sink_parquet_cloud(uri, None, Default::default()).unwrap();

    // let df = LazyFrame::scan_parquet(TEST_S3, args)?
    //     .with_streaming(true)
    //     .select([
    //         // select all columns
    //         all(),
    //     ])
    //     .collect()?;



    dbg!(df);
    Ok(())
}
