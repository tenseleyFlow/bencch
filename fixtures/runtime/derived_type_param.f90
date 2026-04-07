! Derived type passed as subroutine parameter.
! Tests: pass-by-reference, component access on by-ref param.
!
! CHECK: 77
! CHECK: 9.81
program test_param
    implicit none
    type :: point
        integer :: id
        real :: val
    end type
    type(point) :: p
    p%id = 77
    p%val = 9.81
    call show_point(p)
end program

subroutine show_point(p)
    implicit none
    type :: point
        integer :: id
        real :: val
    end type
    type(point) :: p
    print *, p%id
    print *, p%val
end subroutine
