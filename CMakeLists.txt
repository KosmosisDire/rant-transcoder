# The Rant transcoder: one node per host that republishes this host's video as H264.
# CPM fetches Rant and a pinned FFmpeg build.

cmake_minimum_required(VERSION 3.21)

file(STRINGS "${CMAKE_CURRENT_SOURCE_DIR}/VERSION" RANT_TRANSCODER_VERSION LIMIT_COUNT 1)
project(rant_transcoder VERSION ${RANT_TRANSCODER_VERSION} LANGUAGES C CXX)
# a bumped VERSION configures again, so the binary never reports the old one
set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/VERSION")

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Release first so a bare multi config build is Release. Every config lands in bin/.
if(CMAKE_CONFIGURATION_TYPES)
  set(CMAKE_CONFIGURATION_TYPES "Release;Debug;RelWithDebInfo;MinSizeRel"
      CACHE STRING "Build configs (Release first = the default)" FORCE)
endif()
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/bin")
foreach(_cfg ${CMAKE_CONFIGURATION_TYPES})
  string(TOUPPER "${_cfg}" _CFG)
  set(CMAKE_RUNTIME_OUTPUT_DIRECTORY_${_CFG} "${CMAKE_CURRENT_SOURCE_DIR}/bin")
endforeach()

include("${CMAKE_CURRENT_SOURCE_DIR}/cmake/get_cpm.cmake")

# A release pins the newest Rant. The local presets set CPM_rant_SOURCE to ../rant.
CPMAddPackage(NAME rant
  GITHUB_REPOSITORY KosmosisDire/Rant
  VERSION 0.0.32
  EXCLUDE_FROM_ALL YES
  SYSTEM YES)

# Static CRT: a copied exe must run with no VC++ redistributable. The FFmpeg DLLs carry
# their own runtime, so they do not care.
include("${rant_SOURCE_DIR}/tools/static_runtime.cmake")
if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
  add_link_options(-static-libstdc++ -static-libgcc)
endif()

include("${CMAKE_CURRENT_SOURCE_DIR}/cmake/ffmpeg.cmake")
ffmpeg_copy_runtime("${CMAKE_CURRENT_SOURCE_DIR}/bin")

option(RANT_TRANSCODER_TESTS "Build the tests" ${PROJECT_IS_TOP_LEVEL})
if(RANT_TRANSCODER_TESTS)
  enable_testing()
endif()

add_subdirectory(src)

if(RANT_TRANSCODER_TESTS)
  add_subdirectory(tests)
endif()
