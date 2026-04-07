! CHECK: done
! CHECK: 100
program test_string_reassign_loop
    implicit none
    character(:), allocatable :: s
    integer :: i, count

    ! Repeated reassignment in a tight loop — the gfortran ARM64 killer.
    ! Each iteration reallocates: short→long→short→long...
    count = 0
    do i = 1, 100
        if (mod(i, 2) == 0) then
            s = 'short'
        else
            s = 'a longer string value'
        end if
        count = count + 1
    end do
    print *, 'done'
    print *, count
end program
