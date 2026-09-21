use anyhow::Result;

fn main() -> Result<()> {
    recall::init();
    recall::run()
}
