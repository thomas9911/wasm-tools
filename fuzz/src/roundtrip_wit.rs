use arbitrary::{Result, Unstructured};
use std::path::Path;
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{ImportSection, Module};
use wit_component::*;
use wit_parser::{LiftLowerAbi, ManglingAndAbi, PackageId, Resolve};

pub fn run(u: &mut Unstructured<'_>) -> Result<()> {
    let wasm = u.arbitrary().and_then(|config| {
        log::debug!("config: {config:#?}");
        wit_smith::smith(&config, u)
    })?;
    write_file("doc1.wasm", &wasm);
    let (resolve, pkg) = match wit_component::decode(&wasm).unwrap() {
        DecodedWasm::WitPackage(resolve, pkg) => (resolve, pkg),
        DecodedWasm::Component(..) => unreachable!(),
    };
    resolve.assert_valid();

    roundtrip_through_printing("doc1", &resolve, pkg, &wasm);

    let (resolve2, pkg2) = match wit_component::decode(&wasm).unwrap() {
        DecodedWasm::WitPackage(resolve, pkgs) => (resolve, pkgs),
        DecodedWasm::Component(..) => unreachable!(),
    };
    resolve2.assert_valid();

    let wasm2 = wit_component::encode(&resolve2, pkg2).expect("failed to encode WIT document");
    write_file("doc2.wasm", &wasm2);
    roundtrip_through_printing("doc2", &resolve2, pkg2, &wasm2);

    if wasm != wasm2 {
        panic!("roundtrip wasm didn't match");
    }

    // If there's hundreds or thousands of worlds only work with the first few
    // to avoid timing out this fuzzer with asan enabled.
    let mut decoded_bindgens = Vec::new();
    for (id, world) in resolve.worlds.iter().take(20) {
        let mangling = match u.int_in_range(0..=3)? {
            0 => ManglingAndAbi::Legacy(LiftLowerAbi::Sync),
            1 => ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback),
            2 => ManglingAndAbi::Legacy(LiftLowerAbi::AsyncStackful),
            3 => ManglingAndAbi::Standard32,
            _ => unreachable!(),
        };
        log::debug!(
            "embedding world {} as in a dummy module with abi {mangling:?}",
            world.name
        );
        let mut dummy = wit_component::dummy_module(&resolve, id, mangling);
        if u.arbitrary()? {
            let mut dst = Module::default();
            let mut reencode = RemoveImports {
                u,
                removed_funcs: 0,
            };
            if reencode
                .parse_core_module(&mut dst, Default::default(), &dummy)
                .is_ok()
            {
                dummy = dst.finish();
            }
        }
        wit_component::embed_component_metadata(&mut dummy, &resolve, id, StringEncoding::UTF8)
            .unwrap();
        write_file("dummy.wasm", &dummy);

        log::debug!("... componentizing the world into a binary component");
        let wasm = wit_component::ComponentEncoder::default()
            .module(&dummy)
            .unwrap()
            .encode()
            .unwrap();
        write_file("dummy.component.wasm", &wasm);
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&wasm)
            .unwrap();

        // Decode what was just created and record it later for testing merging
        // worlds together.
        let (_, decoded) = wit_component::metadata::decode(&dummy).unwrap();
        decoded_bindgens.push((decoded, dummy, world.name.clone()));

        log::debug!("... decoding the component itself");
        wit_component::decode(&wasm).unwrap();

        // Test out importizing the world and then assert the world is still
        // valid.
        log::debug!("... importizing this world");
        let mut resolve2 = resolve.clone();
        let _ = resolve2.importize(id, None);
    }

    if decoded_bindgens.len() < 2 {
        return Ok(());
    }

    let i = u.choose_index(decoded_bindgens.len())?;
    let (mut b1, wasm1, world1) = decoded_bindgens.swap_remove(i);

    if u.arbitrary()? {
        let i = u.choose_index(decoded_bindgens.len())?;
        let (b2, wasm2, world2) = decoded_bindgens.swap_remove(i);

        log::debug!("merging bindgens world {world1} <- world {world2}");

        write_file("bindgen1.wasm", &wasm1);
        write_file("bindgen2.wasm", &wasm2);

        // Merging worlds may fail but if successful then a `Resolve` is asserted
        // to be valid which is what we're interested in here. Note that failure
        // here can be due to the structure of worlds which aren't reasonable to
        // control in this generator, so it's just done to see what happens and try
        // to trigger panics in `Resolve::assert_valid`.
        let _ = b1.merge(b2);
    } else {
        log::debug!("merging world imports based on semver {world1}");
        write_file("bindgen1.wasm", &wasm1);
        let _ = b1.resolve.merge_world_imports_based_on_semver(b1.world);
    }
    Ok(())
}

fn roundtrip_through_printing(file: &str, resolve: &Resolve, pkg: PackageId, wasm: &[u8]) {
    // Print to a single string, using nested `package ... { .. }` statements,
    // and then parse that in a new `Resolve`.
    let mut new_resolve = Resolve::default();
    new_resolve.all_features = true;
    let package_deps = resolve
        .packages
        .iter()
        .map(|p| p.0)
        .filter(|k| *k != pkg)
        .collect::<Vec<_>>();
    let mut printer = WitPrinter::default();
    printer.print(resolve, pkg, &package_deps).unwrap();
    let doc = printer.output.to_string();
    let new_pkg = new_resolve
        .push_str(&format!("printed-{file}.wit"), &doc)
        .unwrap();

    // Finally encode the `new_resolve` which should be the exact same as
    // before.
    let wasm2 = wit_component::encode(&new_resolve, new_pkg).unwrap();
    write_file(&format!("{file}-reencoded.wasm"), &wasm2);
    if wasm != wasm2 {
        panic!("failed to roundtrip through text printing");
    }
}

fn write_file(path: &str, contents: impl AsRef<[u8]>) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    log::debug!("writing file {path}");
    let contents = contents.as_ref();
    let path = Path::new(path);
    std::fs::write(path, contents).unwrap();
    if path.extension().and_then(|s| s.to_str()) == Some("wasm") {
        let path = path.with_extension("wat");
        log::debug!("writing file {}", path.display());
        std::fs::write(path, wasmprinter::print_bytes(&contents).unwrap()).unwrap();
    }
}

struct RemoveImports<'a, 'b> {
    u: &'a mut Unstructured<'b>,
    removed_funcs: u32,
}

impl Reencode for RemoveImports<'_, '_> {
    type Error = std::convert::Infallible;

    fn function_index(&mut self, idx: u32) -> Result<u32, reencode::Error<Self::Error>> {
        Ok(idx - self.removed_funcs)
    }

    fn parse_import(
        &mut self,
        imports: &mut ImportSection,
        import: wasmparser::Import<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        if self.u.arbitrary().unwrap_or(false) {
            self.removed_funcs += 1;
            Ok(())
        } else {
            reencode::utils::parse_import(self, imports, import)
        }
    }
}

#[should_panic] // TODO: this should get fixed
#[test]
fn smoke() {
    super::test::test_n_times(100, run);
}
