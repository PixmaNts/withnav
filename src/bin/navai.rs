fn main() -> anyhow::Result<()> {
    eprintln!("navai is now nav; this alias is removed in 0.8");
    withnav::app::entry::main()
}
