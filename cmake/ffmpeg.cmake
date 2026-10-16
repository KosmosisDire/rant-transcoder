# FFmpeg 8.1, LGPL, shared: a BtbN/FFmpeg-Builds prebuilt, pinned to a month end build since
# those are kept. macOS has none there, so it takes Homebrew's through pkg-config.

# ffmpeg::avformat, avcodec, swscale and avutil, and ffmpeg_copy_runtime(dir), which puts the
# shared libraries beside the exes in dir.

set(FFMPEG_BUILD "autobuild-2026-09-30-13-08")
set(FFMPEG_NAME "ffmpeg-n8.1.3-9-g29e619e767")
set(FFMPEG_LIBS avformat avcodec swscale avutil)

if(APPLE)
  find_package(PkgConfig REQUIRED)
  pkg_check_modules(FFMPEG_PC REQUIRED IMPORTED_TARGET libavformat libavcodec libswscale libavutil)
  foreach(lib ${FFMPEG_LIBS})
    add_library(ffmpeg::${lib} INTERFACE IMPORTED)
    target_link_libraries(ffmpeg::${lib} INTERFACE PkgConfig::FFMPEG_PC)
  endforeach()
  function(ffmpeg_copy_runtime dir)
  endfunction()
  return()
endif()

if(WIN32)
  set(_asset "${FFMPEG_NAME}-win64-lgpl-shared-8.1.zip")
elseif(CMAKE_SYSTEM_PROCESSOR MATCHES "aarch64|arm64")
  set(_asset "${FFMPEG_NAME}-linuxarm64-lgpl-shared-8.1.tar.xz")
else()
  set(_asset "${FFMPEG_NAME}-linux64-lgpl-shared-8.1.tar.xz")
endif()

CPMAddPackage(NAME ffmpeg
  URL "https://github.com/BtbN/FFmpeg-Builds/releases/download/${FFMPEG_BUILD}/${_asset}"
  VERSION 8.1.3
  DOWNLOAD_ONLY YES)

foreach(lib ${FFMPEG_LIBS})
  add_library(ffmpeg::${lib} SHARED IMPORTED)
  set_target_properties(ffmpeg::${lib} PROPERTIES
    INTERFACE_INCLUDE_DIRECTORIES "${ffmpeg_SOURCE_DIR}/include")
  if(WIN32)
    file(GLOB _dll "${ffmpeg_SOURCE_DIR}/bin/${lib}-*.dll")
    set_target_properties(ffmpeg::${lib} PROPERTIES
      IMPORTED_IMPLIB "${ffmpeg_SOURCE_DIR}/lib/${lib}.lib"
      IMPORTED_LOCATION "${_dll}")
  else()
    set_target_properties(ffmpeg::${lib} PROPERTIES
      IMPORTED_LOCATION "${ffmpeg_SOURCE_DIR}/lib/lib${lib}.so")
  endif()
endforeach()

# An exe finds the copies beside it: Windows looks there first, Linux through the rpath.
if(NOT WIN32)
  set(CMAKE_BUILD_RPATH "$ORIGIN")
  set(CMAKE_INSTALL_RPATH "$ORIGIN")
  # an RPATH, unlike a RUNPATH, also serves the libraries' own needs, such as avcodec's swresample
  add_link_options("LINKER:--disable-new-dtags")
endif()

function(ffmpeg_copy_runtime dir)
  if(WIN32)
    file(GLOB _files "${ffmpeg_SOURCE_DIR}/bin/*.dll")
  else()
    file(GLOB _files "${ffmpeg_SOURCE_DIR}/lib/*.so*")
  endif()
  file(COPY ${_files} DESTINATION "${dir}")
endfunction()
