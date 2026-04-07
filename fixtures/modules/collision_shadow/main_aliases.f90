! CHECK: 11
program main
    use collision_left_values, only: left_value => payload
    use collision_right_values, only: right_value => payload
    implicit none

    print *, left_value + right_value
end program main
