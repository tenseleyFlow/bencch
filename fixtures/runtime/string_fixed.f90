! CHECK: Fortran
! CHECK: Hi
program test_string_fixed
    implicit none
    character(10) :: greeting
    character(5) :: short

    greeting = 'Fortran'
    print *, greeting

    short = 'Hi there, world'
    print *, short
end program
