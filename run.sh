LD_LIBRARY_PATH=/home/ubuntu/dam_ramulator/external/ramulator2_wrapper/ext/ramulator2 \
cargo test --package dam_ramulator --lib -- tests::test_ramulator_wrapper::test::instantiate_wrapper \
--exact --show-output

LD_LIBRARY_PATH=/home/ubuntu/dam_ramulator/external/ramulator2_wrapper/ext/ramulator2 \
cargo test --package dam_ramulator --lib -- ramulator::ramulator_context::test::read_with_logging --exact --show-output