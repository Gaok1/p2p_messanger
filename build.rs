fn main() {
    println!("cargo:rerun-if-changed=hollow.ico");

    #[cfg(windows)]
    {
        use std::path::PathBuf;

        if let Err(err) = winres::WindowsResource::new()
            .set_icon("hollow.ico")
            .compile()
        {
            println!("cargo:warning=winres: falha ao aplicar hollow.ico: {err}");
            return;
        }

        // No toolchain GNU no Windows, o `winres` gera `resource.o` dentro de um
        // `libresource.a`. Como o objeto não define símbolos, o linker pode não
        // puxar o `.o` do `.a`, resultando em um `.exe` sem `.rsrc`.
        //
        // Força o link do objeto diretamente.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            if let Some(out_dir) = std::env::var_os("OUT_DIR") {
                let resource_obj = PathBuf::from(out_dir).join("resource.o");
                if resource_obj.exists() {
                    println!("cargo:rustc-link-arg={}", resource_obj.display());
                } else {
                    println!(
                        "cargo:warning=winres: resource.o não encontrado: {}",
                        resource_obj.display()
                    );
                }
            }
        }
    }
}
