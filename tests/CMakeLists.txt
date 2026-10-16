# unit.<name>  one C++ executable per module, tests/unit/<name>.cpp
# e2e          a camera, the transcoder and a viewer in one process, tests/e2e/transcode.cpp

set(CMAKE_RUNTIME_OUTPUT_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}/bin")
foreach(_cfg ${CMAKE_CONFIGURATION_TYPES})
  string(TOUPPER "${_cfg}" _CFG)
  set(CMAKE_RUNTIME_OUTPUT_DIRECTORY_${_CFG} "${CMAKE_CURRENT_BINARY_DIR}/bin")
endforeach()
ffmpeg_copy_runtime("${CMAKE_CURRENT_BINARY_DIR}/bin")

foreach(name rules)
  add_executable(unit_${name} unit/${name}.cpp)
  target_include_directories(unit_${name} PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
  target_link_libraries(unit_${name} PRIVATE rant_transcoder_core)
  add_test(NAME unit.${name} COMMAND unit_${name})
endforeach()

add_executable(e2e_transcode e2e/transcode.cpp)
target_include_directories(e2e_transcode PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(e2e_transcode PRIVATE rant_transcoder_core)
add_test(NAME e2e COMMAND e2e_transcode)
set_tests_properties(e2e PROPERTIES TIMEOUT 120)
