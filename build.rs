fn main() {
    #[cfg(windows)]
    embed_resource::compile("quarkdrive.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
