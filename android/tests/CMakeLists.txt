add_library(tm_test_framework STATIC framework.cpp)
target_include_directories(tm_test_framework PUBLIC ${CMAKE_CURRENT_SOURCE_DIR})
target_link_libraries(tm_test_framework PUBLIC tm_core)

file(GLOB TM_UNIT_SOURCES CONFIGURE_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/unit/*.cpp)
add_executable(tm_unit_tests ${TM_UNIT_SOURCES})
target_link_libraries(tm_unit_tests PRIVATE tm_test_framework)
add_test(NAME unit COMMAND tm_unit_tests)

file(GLOB TM_INTEGRATION_SOURCES CONFIGURE_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/integration/*.cpp)
if(TM_INTEGRATION_SOURCES)
  add_executable(tm_integration_tests ${TM_INTEGRATION_SOURCES})
  target_link_libraries(tm_integration_tests PRIVATE tm_test_framework)
  target_compile_definitions(tm_integration_tests PRIVATE
    TM_REPO_ROOT="${CMAKE_CURRENT_SOURCE_DIR}/..")
  # The fake relay needs a Python interpreter; the test skips itself without one.
  add_test(NAME integration COMMAND tm_integration_tests)
  set_tests_properties(integration PROPERTIES TIMEOUT 300)
endif()

if(TM_BUILD_FUZZERS)
  file(GLOB TM_FUZZ_SOURCES CONFIGURE_DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/fuzz/fuzz_*.cpp)
  # fuzz_main.cpp is the shared corpus driver, not a target of its own.
  list(REMOVE_ITEM TM_FUZZ_SOURCES ${CMAKE_CURRENT_SOURCE_DIR}/fuzz/fuzz_main.cpp)
  foreach(fuzz_source ${TM_FUZZ_SOURCES})
    get_filename_component(fuzz_name ${fuzz_source} NAME_WE)
    add_executable(${fuzz_name} ${fuzz_source} ${CMAKE_CURRENT_SOURCE_DIR}/fuzz/fuzz_main.cpp)
    target_include_directories(${fuzz_name} PRIVATE ${CMAKE_CURRENT_SOURCE_DIR})
    target_link_libraries(${fuzz_name} PRIVATE tm_core)
    # Each driver runs a bounded deterministic corpus in CI; the same entry point
    # links against libFuzzer when it is available (see fuzz/README.md).
    add_test(NAME ${fuzz_name} COMMAND ${fuzz_name} --iterations=2000)
    set_tests_properties(${fuzz_name} PROPERTIES TIMEOUT 300)
  endforeach()
endif()
