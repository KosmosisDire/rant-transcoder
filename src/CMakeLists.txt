# Everything but main() is one static library, so the tests link the same code.

# The one RANT_IMPLEMENTATION anchor, on its own so warnings apply to our code only.
add_library(rant_impl STATIC "${rant_SOURCE_DIR}/dist/rant.cpp")
target_link_libraries(rant_impl PUBLIC rant::rant rant::rant_platform)

add_library(rant_transcoder_core STATIC
  hosts.cpp
  log.cpp
  media.cpp
  rules.cpp
  source.cpp
  stream.cpp
  transcoder.cpp)
target_include_directories(rant_transcoder_core PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(rant_transcoder_core PUBLIC rant_impl
                      ffmpeg::avformat ffmpeg::avcodec ffmpeg::swscale ffmpeg::avutil)
target_compile_definitions(rant_transcoder_core PUBLIC RANT_TRANSCODER_VERSION="${PROJECT_VERSION}")
if(WIN32)
  target_link_libraries(rant_transcoder_core PUBLIC iphlpapi ws2_32)
else()
  target_link_libraries(rant_transcoder_core PUBLIC ${CMAKE_DL_LIBS})
endif()
if(MSVC)
  target_compile_options(rant_transcoder_core PRIVATE /W4 /permissive-)
  # FFmpeg's headers mark old fields deprecated, and its inline helpers narrow
  target_compile_options(rant_transcoder_core PRIVATE /wd4996 /wd4244)
else()
  target_compile_options(rant_transcoder_core PRIVATE -Wall -Wextra)
endif()

add_executable(rant-transcoder main.cpp)
target_link_libraries(rant-transcoder PRIVATE rant_transcoder_core)
