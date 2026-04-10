# Build codec2 WASM via emscripten
#
# Prerequisites: emsdk activated, codec2 source checked out
#
# Usage:
#   make            - build codec2-wasm.js + codec2-wasm.wasm
#   make clean      - remove build artifacts
#   make fetch      - clone codec2 source

CODEC2_DIR = codec2
CODEC2_BUILD = $(CODEC2_DIR)/build
OUTPUT = ../../src/lib/voice/codec2-wasm.js

EMCC_FLAGS = -O2 \
	-s MODULARIZE=1 \
	-s EXPORT_ES6=1 \
	-s ALLOW_MEMORY_GROWTH=0 \
	-s INITIAL_MEMORY=1048576 \
	-s EXPORTED_FUNCTIONS='["_codec2_create","_codec2_destroy","_codec2_encode","_codec2_decode","_codec2_samples_per_frame","_codec2_bits_per_frame","_malloc","_free"]' \
	-s EXPORTED_RUNTIME_METHODS='["HEAP16","HEAPU8"]' \
	-I $(CODEC2_DIR)/src

.PHONY: all clean fetch

all: $(OUTPUT)

$(OUTPUT): codec2_glue.c $(CODEC2_BUILD)/src/libcodec2.a
	emcc codec2_glue.c \
		$(EMCC_FLAGS) \
		-L $(CODEC2_BUILD)/src -lcodec2 \
		-o $(OUTPUT)
	@echo "Built: $(OUTPUT) + codec2-wasm.wasm"

$(CODEC2_BUILD)/src/libcodec2.a: $(CODEC2_DIR)/CMakeLists.txt
	mkdir -p $(CODEC2_BUILD)
	cd $(CODEC2_BUILD) && emcmake cmake .. -DCMAKE_BUILD_TYPE=Release
	cd $(CODEC2_BUILD) && emmake make codec2

fetch:
	git clone --depth 1 --branch v1.2.0 https://github.com/drowe67/codec2.git $(CODEC2_DIR)

clean:
	rm -rf $(CODEC2_BUILD) $(OUTPUT) $(OUTPUT:.js=.wasm)
