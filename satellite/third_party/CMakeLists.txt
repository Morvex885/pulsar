# Builds sendspin-cpp and its dependencies as static libraries for pulsar.
# Sources come from third_party/src/ (populated by fetch.sh); nothing is downloaded here.
# Driven by the top-level Makefile; see the "third_party" rule there.
cmake_minimum_required(VERSION 3.16)
project(cucumber_sendspin_deps C CXX)

set(_src ${CMAKE_CURRENT_SOURCE_DIR}/src)
set(FETCHCONTENT_FULLY_DISCONNECTED ON CACHE BOOL "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_ARDUINOJSON ${_src}/ArduinoJson CACHE PATH "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_MICRO_FLAC ${_src}/micro-flac CACHE PATH "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_MICRO_OPUS ${_src}/micro-opus CACHE PATH "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_IXWEBSOCKET ${_src}/IXWebSocket CACHE PATH "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_NOISE_C ${_src}/noise-c CACHE PATH "" FORCE)

# Roles: player (with Opus), metadata, controller (transport commands) and visualizer (LED music
# visualization). Keep in sync with ROLES in crates/pulsar-sendspin/build.rs.
set(SENDSPIN_ENABLE_PLAYER ON CACHE BOOL "" FORCE)
set(SENDSPIN_ENABLE_OPUS ON CACHE BOOL "" FORCE)
set(SENDSPIN_ENABLE_METADATA ON CACHE BOOL "" FORCE)
set(SENDSPIN_ENABLE_CONTROLLER ON CACHE BOOL "" FORCE)
set(SENDSPIN_ENABLE_COLOR OFF CACHE BOOL "" FORCE)
set(SENDSPIN_ENABLE_ARTWORK OFF CACHE BOOL "" FORCE)
set(SENDSPIN_ENABLE_VISUALIZER ON CACHE BOOL "" FORCE)
set(SENDSPIN_ENABLE_SOURCE OFF CACHE BOOL "" FORCE)
set(BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
set(SENDSPIN_USE_CCACHE OFF CACHE BOOL "" FORCE)
set(MICRO_FLAC_ENABLE_OGG OFF CACHE BOOL "" FORCE)
set(USE_TLS OFF CACHE BOOL "" FORCE)
set(USE_ZLIB OFF CACHE BOOL "" FORCE)
set(IXWEBSOCKET_INSTALL OFF CACHE BOOL "" FORCE)

add_subdirectory(${_src}/sendspin-cpp sendspin)
