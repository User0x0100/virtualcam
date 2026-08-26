cmake_minimum_required(VERSION 3.24)
project(virtualcam_managed LANGUAGES CXX)

if(MSVC)
    set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
endif()

add_library(virtualcam-managed SHARED
    managed-virtualcam.cpp
    third_party/libdshowcapture/source/output-filter.cpp
    third_party/libdshowcapture/source/dshow-formats.cpp
    third_party/libdshowcapture/source/dshow-media-type.cpp
    exports.def
)

target_compile_features(virtualcam-managed PRIVATE cxx_std_17)
target_compile_definitions(virtualcam-managed PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
target_compile_options(virtualcam-managed PRIVATE /W4 /permissive- /EHsc)
target_link_libraries(virtualcam-managed PRIVATE ole32 oleaut32 advapi32 strmiids)

set_target_properties(virtualcam-managed PROPERTIES
    OUTPUT_NAME "virtualcam-managed"
    PREFIX ""
)

include(CTest)
if(BUILD_TESTING)
    add_executable(virtualcam-managed-fixed-mode-test
        tests/fixed-mode-test.cpp
        third_party/libdshowcapture/source/output-filter.cpp
        third_party/libdshowcapture/source/dshow-formats.cpp
        third_party/libdshowcapture/source/dshow-media-type.cpp
    )
    target_compile_features(virtualcam-managed-fixed-mode-test PRIVATE cxx_std_17)
    target_compile_definitions(virtualcam-managed-fixed-mode-test PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
    target_compile_options(virtualcam-managed-fixed-mode-test PRIVATE /W4 /permissive- /EHsc)
    target_link_libraries(virtualcam-managed-fixed-mode-test PRIVATE ole32 oleaut32 strmiids)
    add_test(NAME virtualcam-managed-fixed-mode COMMAND virtualcam-managed-fixed-mode-test)
endif()
