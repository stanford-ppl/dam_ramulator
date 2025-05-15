LD_LIBRARY_PATH=/home/tanmayg/dam_ramulator/external/ramulator2_wrapper/ext/ramulator2 \
cargo test --package dam_ramulator --lib -- tests::test_ramulator_wrapper::test::instantiate_wrapper \
--exact --show-output